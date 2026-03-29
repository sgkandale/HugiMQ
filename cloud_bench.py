import boto3
import time
import os
import paramiko
import sys
import threading
from botocore.exceptions import ClientError
from dotenv import load_dotenv
import logging

# Load environment variables from .env file
load_dotenv()

# --- Configuration ---
AWS_ACCESS_KEY = os.getenv('AWS_ACCESS_KEY')
AWS_SECRET_KEY = os.getenv('AWS_SECRET_KEY')
AWS_REGION = os.getenv('AWS_REGION', 'us-east-1')
AMI_ID = os.getenv('AMI_ID', 'ami-0c7217cdde317cfec')
KEY_NAME = os.getenv('KEY_NAME', 'hugimq-bench-key')
KEY_PATH = os.getenv('KEY_PATH', './hugimq-bench-key.pem')

# Validate required variables
if not AWS_ACCESS_KEY or not AWS_SECRET_KEY:
    print("Error: AWS_ACCESS_KEY and AWS_SECRET_KEY must be set in your .env file.")
    sys.exit(1)

# Instance Configurations
BENCHMARKER_TYPE = 'c6i.2xlarge'
REDIS_TYPE = 'c6i.xlarge'
SECURITY_GROUP_NAME = 'hugimq-bench-sg'
REPO_URL = os.getenv('REPO_URL', 'https://github.com/sgkandale/HugiMQ.git')

# --- Logging Setup ---
logging.basicConfig(
    level=logging.INFO,
    format='%(asctime)s - %(levelname)s - %(message)s',
    datefmt='%H:%M:%S'
)
log = logging.getLogger("cloud_bench")

# --- AWS Setup ---
ec2 = boto3.client(
    'ec2',
    aws_access_key_id=AWS_ACCESS_KEY,
    aws_secret_access_key=AWS_SECRET_KEY,
    region_name=AWS_REGION
)

def create_key_pair():
    aws_exists = False
    try:
        ec2.describe_key_pairs(KeyNames=[KEY_NAME])
        aws_exists = True
    except ClientError as e:
        if 'InvalidKeyPair.NotFound' not in str(e):
            raise e
    
    file_exists = os.path.exists(KEY_PATH)

    if aws_exists:
        if file_exists:
            log.info(f"Key pair '{KEY_NAME}' and local file '{KEY_PATH}' already exist. Using existing key.")
            os.chmod(KEY_PATH, 0o400)
            return
        else:
            log.error(f"Error: Key pair '{KEY_NAME}' exists in AWS but local file '{KEY_PATH}' was not found.")
            sys.exit(1)
    else:
        if file_exists:
            log.error(f"Error: Local file '{KEY_PATH}' exists but key pair '{KEY_NAME}' does not exist in AWS.")
            sys.exit(1)

        log.info(f"Creating key pair: {KEY_NAME}")
        response = ec2.create_key_pair(KeyName=KEY_NAME)
        key_material = response['KeyMaterial']
        
        with open(KEY_PATH, 'w') as f:
            f.write(key_material)
        
        os.chmod(KEY_PATH, 0o400)
        log.info(f"Key pair created and saved to {KEY_PATH}")

def create_security_group():
    try:
        response = ec2.create_security_group(
            GroupName=SECURITY_GROUP_NAME,
            Description='Security group for HugiMQ benchmarking'
        )
        sg_id = response['GroupId']
        ec2.authorize_security_group_ingress(
            GroupId=sg_id,
            IpPermissions=[
                {'IpProtocol': 'tcp', 'FromPort': 22, 'ToPort': 22, 'IpRanges': [{'CidrIp': '0.0.0.0/0'}]},
                {'IpProtocol': 'tcp', 'FromPort': 6379, 'ToPort': 6379, 'IpRanges': [{'CidrIp': '0.0.0.0/0'}]}
            ]
        )
        return sg_id
    except ClientError as e:
        if 'InvalidGroup.Duplicate' in str(e):
            response = ec2.describe_security_groups(GroupNames=[SECURITY_GROUP_NAME])
            return response['SecurityGroups'][0]['GroupId']
        raise e

def get_target_subnet():
    # Get subnets from the default VPC
    vpcs = ec2.describe_vpcs(Filters=[{'Name': 'isDefault', 'Values': ['true']}])
    if not vpcs['Vpcs']:
        subnets = ec2.describe_subnets()
    else:
        vpc_id = vpcs['Vpcs'][0]['VpcId']
        subnets = ec2.describe_subnets(Filters=[{'Name': 'vpc-id', 'Values': [vpc_id]}])
    
    if not subnets['Subnets']:
        raise Exception("No subnets found in the target VPC.")
    
    # Just pick the first one
    target = subnets['Subnets'][0]
    log.info(f"Targeting Subnet: {target['SubnetId']} ({target['AvailabilityZone']})")
    return target['SubnetId']

def request_spot_instance(instance_type, name, subnet_id):
    log.info(f"Requesting {name} ({instance_type}) with higher spot bid in {subnet_id}...")
    response = ec2.run_instances(
        ImageId=AMI_ID,
        InstanceType=instance_type,
        KeyName=KEY_NAME,
        SecurityGroupIds=[create_security_group()],
        SubnetId=subnet_id,
        InstanceMarketOptions={
            'MarketType': 'spot',
            'SpotOptions': {
                'MaxPrice': '0.30',
                'SpotInstanceType': 'one-time',
                'InstanceInterruptionBehavior': 'terminate',
            }
        },
        MinCount=1,
        MaxCount=1,
        TagSpecifications=[{
            'ResourceType': 'instance',
            'Tags': [{'Key': 'Name', 'Value': name}]
        }]
    )
    return response['Instances'][0]['InstanceId']

def wait_for_instance(instance_id, stop_event):
    log.info(f"Waiting for {instance_id} to be ready (status checks)...")
    while not stop_event.is_set():
        response = ec2.describe_instances(InstanceIds=[instance_id])
        state = response['Reservations'][0]['Instances'][0]['State']['Name']
        if state == 'running':
            break
        if state in ['terminated', 'shutting-down']:
            raise Exception(f"Instance {instance_id} terminated while waiting.")
        time.sleep(5)
    
    if stop_event.is_set():
        raise Exception("Wait cancelled by stop event.")

    status_waiter = ec2.get_waiter('instance_status_ok')
    status_waiter.wait(InstanceIds=[instance_id])
    
    response = ec2.describe_instances(InstanceIds=[instance_id])
    instance = response['Reservations'][0]['Instances'][0]
    return instance['PublicIpAddress'], instance['PrivateIpAddress']

def run_ssh_commands(ip, commands, prefix, stop_event):
    client = paramiko.SSHClient()
    client.set_missing_host_key_policy(paramiko.AutoAddPolicy())
    
    for i in range(20):
        if stop_event.is_set(): return
        try:
            client.connect(hostname=ip, username='ubuntu', key_filename=KEY_PATH, timeout=10)
            break
        except Exception as e:
            log.info(f"[{prefix}] Retrying SSH connection to {ip}... ({i+1}/20)")
            time.sleep(10)
            if i == 19:
                raise Exception(f"Failed to connect to {ip} after 20 retries: {e}")
    
    for cmd in commands:
        if stop_event.is_set(): break
        log.info(f"[{prefix}] Executing: {cmd}")
        stdin, stdout, stderr = client.exec_command(cmd)
        
        channel = stdout.channel
        while not channel.exit_status_ready():
            if stop_event.is_set(): 
                channel.close()
                return
            if channel.recv_ready():
                output = channel.recv(1024).decode('utf-8')
                for line in output.splitlines():
                    log.info(f"[{prefix}] {line.strip()}")
            
            if channel.recv_stderr_ready():
                output_err = channel.recv_stderr(1024).decode('utf-8')
                for line in output_err.splitlines():
                    log.warning(f"[{prefix}] [STDERR] {line.strip()}")
            
            time.sleep(0.1)

        while channel.recv_ready():
            output = channel.recv(1024).decode('utf-8')
            for line in output.splitlines():
                log.info(f"[{prefix}] {line.strip()}")
        
        while channel.recv_stderr_ready():
            output_err = channel.recv_stderr(1024).decode('utf-8')
            for line in output_err.splitlines():
                log.warning(f"[{prefix}] [STDERR] {line.strip()}")
            
        exit_status = channel.recv_exit_status()
        if exit_status != 0:
            raise Exception(f"Command '{cmd}' failed on {ip} with exit status {exit_status}")
            
    client.close()

# --- Main Logic ---

def main():
    redis_id = None
    bench_id = None
    stop_event = threading.Event()

    try:
        create_key_pair()
        subnet_id = get_target_subnet()
        redis_id = request_spot_instance(REDIS_TYPE, 'HugiMQ-Redis-Target', subnet_id)
        bench_id = request_spot_instance(BENCHMARKER_TYPE, 'HugiMQ-Benchmarker', subnet_id)
        
        ips = {}
        errors = []
        def get_ips(id, key):
            try:
                ips[key] = wait_for_instance(id, stop_event)
            except Exception as e:
                errors.append(f"Error waiting for {key} instance ({id}): {e}")

        t1 = threading.Thread(target=get_ips, args=(redis_id, 'redis'))
        t2 = threading.Thread(target=get_ips, args=(bench_id, 'bench'))
        t1.start()
        t2.start()
        t1.join()
        t2.join()

        if errors or stop_event.is_set():
            for err in errors: log.error(err)
            return

        redis_pub, redis_priv = ips['redis']
        bench_pub, bench_priv = ips['bench']
        
        log.info(f"Redis (Public: {redis_pub}, Private: {redis_priv})")
        log.info(f"Benchmarker (Public: {bench_pub}, Private: {bench_priv})")

        def monitor_instances():
            while not stop_event.is_set():
                try:
                    status = ec2.describe_instances(InstanceIds=[redis_id, bench_id])
                    for res in status['Reservations']:
                        for inst in res['Instances']:
                            if inst['State']['Name'] in ['shutting-down', 'terminated']:
                                log.error(f"CRITICAL: Instance {inst['InstanceId']} terminated unexpectedly.")
                                stop_event.set()
                                return
                except Exception: pass
                time.sleep(10)

        monitor_thread = threading.Thread(target=monitor_instances, daemon=True)
        monitor_thread.start()

        redis_setup = [
            "export DEBIAN_FRONTEND=noninteractive && sudo -E apt-get update",
            "export DEBIAN_FRONTEND=noninteractive && sudo -E apt-get install -y redis-server",
            "sudo sed -i 's/bind 127.0.0.1/bind 0.0.0.0/' /etc/redis/redis.conf && " + \
            "sudo sed -i 's/protected-mode yes/protected-mode no/' /etc/redis/redis.conf && " + \
            "sudo sed -i 's/# io-threads 4/io-threads 2/' /etc/redis/redis.conf && " + \
            "sudo sed -i 's/# io-threads-do-reads no/io-threads-do-reads yes/' /etc/redis/redis.conf && " + \
            "sudo systemctl restart redis-server"
        ]
        
        bench_setup = [
            "export DEBIAN_FRONTEND=noninteractive && sudo -E apt-get update",
            "export DEBIAN_FRONTEND=noninteractive && sudo -E apt-get install -y build-essential curl git",
            "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y",
            f"source $HOME/.cargo/env && git clone {REPO_URL} hugimq_scratch && cd hugimq_scratch && cargo build --release -p benchmarker"
        ]

        t1 = threading.Thread(target=run_ssh_commands, args=(redis_pub, redis_setup, "REDIS", stop_event))
        t2 = threading.Thread(target=run_ssh_commands, args=(bench_pub, bench_setup, "BENCH", stop_event))
        t1.start()
        t2.start()
        t1.join()
        t2.join()

        if stop_event.is_set(): return
        # Run Benchmark
        benchmark_cmd = [
            f"source $HOME/.cargo/env && cd hugimq_scratch && ./target/release/benchmarker redis --connections 20 --messages-per-conn 50000 --payload-size 128 --redis-url redis://{redis_priv}/ > results.txt",
            "cat hugimq_scratch/results.txt"
        ]

        run_ssh_commands(bench_pub, benchmark_cmd, "BENCHMARK-RUN", stop_event)
        log.info("Benchmark complete.")

    except KeyboardInterrupt:
        log.info("Caught KeyboardInterrupt, terminating...")
        stop_event.set()
    except Exception as e:
        log.error(f"Error occurred: {e}")
    finally:
        if redis_id or bench_id:
            ids = [i for i in [redis_id, bench_id] if i]
            log.info(f"Terminating instances: {ids}")
            ec2.terminate_instances(InstanceIds=ids)
            log.info("Instances terminated.")

if __name__ == '__main__':
    main()
