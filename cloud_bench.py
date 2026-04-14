import boto3
import time
import os
import paramiko
import sys
import threading
import signal
from botocore.exceptions import ClientError
from dotenv import load_dotenv
import logging
import argparse

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
TARGET_TYPE = 'c6i.xlarge'
SECURITY_GROUP_NAME = 'hugimq-bench-sg'
REPO_URL = os.getenv('REPO_URL', 'https://github.com/sgkandale/HugiMQ.git')

# --- Logging Setup ---
logging.basicConfig(
    level=logging.INFO,
    format='%(asctime)s - %(levelname)s - %(message)s',
    datefmt='%H:%M:%S'
)
log = logging.getLogger("cloud_bench")

# --- Heartbeat to prevent CLI timeouts ---
def heartbeat(stop_event):
    while not stop_event.is_set():
        time.sleep(30)
        if not stop_event.is_set():
            # Print a single dot to stdout to maintain activity
            sys.stdout.write(".")
            sys.stdout.flush()

# --- AWS Setup ---
ec2 = boto3.client(
    'ec2',
    aws_access_key_id=AWS_ACCESS_KEY,
    aws_secret_access_key=AWS_SECRET_KEY,
    region_name=AWS_REGION
)

class InstanceManager:
    def __init__(self):
        self.instances = []
        self.lock = threading.Lock()

    def add(self, instance_id):
        with self.lock:
            if instance_id not in self.instances:
                self.instances.append(instance_id)

    def terminate_all(self):
        with self.lock:
            if not self.instances:
                return
            log.info(f"Terminating instances: {self.instances}")
            try:
                response = ec2.terminate_instances(InstanceIds=self.instances)
                for term in response.get('TerminatingInstances', []):
                    log.info(f"Instance {term['InstanceId']} state: {term['CurrentState']['Name']}")
                
                # Wait up to 30 seconds for confirmation of shutting-down state
                log.info("Waiting for AWS to confirm shutting-down status...")
                for _ in range(6):
                    time.sleep(5)
                    try:
                        check = ec2.describe_instances(InstanceIds=self.instances)
                        states = [i['State']['Name'] for r in check['Reservations'] for i in r['Instances']]
                        log.info(f"Current states: {states}")
                        if all(s in ['shutting-down', 'terminated'] for s in states):
                            break
                    except:
                        break
                
                self.instances = []
            except Exception as e:
                log.error(f"Failed to terminate instances: {e}")

manager = InstanceManager()

def cleanup_zombies():
    """Finds and kills any HugiMQ-tagged instances from previous runs that are still alive."""
    log.info("Checking for zombie instances from previous runs...")
    try:
        res = ec2.describe_instances(Filters=[
            {'Name': 'tag:Name', 'Values': ['HugiMQ-*']},
            {'Name': 'instance-state-name', 'Values': ['running', 'pending', 'stopping', 'stopped']}
        ])
        ids = [i['InstanceId'] for r in res['Reservations'] for i in r['Instances']]
        if ids:
            log.warning(f"Found {len(ids)} zombie instances. Terminating: {ids}")
            ec2.terminate_instances(InstanceIds=ids)
            for i in ids: manager.add(i)
            manager.terminate_all()
        else:
            log.info("No zombies found.")
    except Exception as e:
        log.error(f"Zombie cleanup failed: {e}")

def signal_handler(signum, frame):
    log.info(f"Signal {signum} received. Cleaning up...")
    manager.terminate_all()
    sys.exit(1)

signal.signal(signal.SIGINT, signal_handler)
signal.signal(signal.SIGTERM, signal_handler)

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
            log.info(f"Key pair '{KEY_NAME}' and local file '{KEY_PATH}' already exist.")
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
                {'IpProtocol': 'tcp', 'FromPort': 6379, 'ToPort': 6379, 'IpRanges': [{'CidrIp': '0.0.0.0/0'}]},
                {'IpProtocol': 'udp', 'FromPort': 6379, 'ToPort': 6379, 'IpRanges': [{'CidrIp': '0.0.0.0/0'}]},
            ]
        )
        return sg_id
    except ClientError as e:
        if 'InvalidGroup.Duplicate' in str(e):
            response = ec2.describe_security_groups(GroupNames=[SECURITY_GROUP_NAME])
            return response['SecurityGroups'][0]['GroupId']
        raise e

def get_target_subnet(az=None):
    vpcs = ec2.describe_vpcs(Filters=[{'Name': 'isDefault', 'Values': ['true']}])
    if not vpcs['Vpcs']:
        subnets = ec2.describe_subnets()
    else:
        vpc_id = vpcs['Vpcs'][0]['VpcId']
        subnets = ec2.describe_subnets(Filters=[{'Name': 'vpc-id', 'Values': [vpc_id]}])

    if not subnets['Subnets']:
        raise Exception("No subnets found in the target VPC.")

    if az:
        for subnet in subnets['Subnets']:
            if subnet['AvailabilityZone'] == az:
                target = subnet
                break
        else:
            raise Exception(f"No subnet found in AZ: {az}")
    else:
        target = subnets['Subnets'][0]
    
    log.info(f"Targeting Subnet: {target['SubnetId']} ({target['AvailabilityZone']})")
    return target['SubnetId']

def request_spot_instance(instance_type, name, subnet_id):
    log.info(f"Requesting {name} ({instance_type}) spot instance...")
    response = ec2.run_instances(
        ImageId=AMI_ID,
        InstanceType=instance_type,
        KeyName=KEY_NAME,
        SecurityGroupIds=[create_security_group()],
        SubnetId=subnet_id,
        InstanceMarketOptions={
            'MarketType': 'spot',
            'SpotOptions': {
                'MaxPrice': '0.50',
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
    instance_id = response['Instances'][0]['InstanceId']
    manager.add(instance_id)
    return instance_id

def wait_for_instance(instance_id, stop_event):
    log.info(f"Waiting for {instance_id} to be ready...")
    while not stop_event.is_set():
        response = ec2.describe_instances(InstanceIds=[instance_id])
        state = response['Reservations'][0]['Instances'][0]['State']['Name']
        if state == 'running':
            break
        time.sleep(5)
    
    status_waiter = ec2.get_waiter('instance_status_ok')
    status_waiter.wait(InstanceIds=[instance_id])
    
    response = ec2.describe_instances(InstanceIds=[instance_id])
    instance = response['Reservations'][0]['Instances'][0]
    return instance['PublicIpAddress'], instance['PrivateIpAddress']

def run_ssh_commands(ip, commands, prefix, stop_event):
    client = paramiko.SSHClient()
    client.set_missing_host_key_policy(paramiko.AutoAddPolicy())
    
    connected = False
    for i in range(20):
        if stop_event.is_set(): return
        try:
            client.connect(hostname=ip, username='ubuntu', key_filename=KEY_PATH, timeout=10)
            connected = True
            break
        except Exception:
            log.info(f"[{prefix}] Retrying SSH connection to {ip}... ({i+1}/20)")
            time.sleep(10)
    
    if not connected:
        raise Exception(f"Failed to connect to {ip}")

    for cmd in commands:
        if stop_event.is_set(): break
        log.info(f"[{prefix}] Executing: {cmd}")
        stdin, stdout, stderr = client.exec_command(cmd)
        
        def pipe_stream(stream, is_stderr=False):
            for line in stream:
                if stop_event.is_set(): break
                if is_stderr:
                    log.warning(f"[{prefix}] [STDERR] {line.strip()}")
                else:
                    log.info(f"[{prefix}] {line.strip()}")

        t1 = threading.Thread(target=pipe_stream, args=(stdout,))
        t2 = threading.Thread(target=pipe_stream, args=(stderr, True))
        t1.start()
        t2.start()
        t1.join()
        t2.join()
        
        exit_status = stdout.channel.recv_exit_status()
        if exit_status != 0:
            log.error(f"[{prefix}] Command failed with status {exit_status}.")
            raise Exception(f"Command failed on {ip}")
            
    client.close()

def main():
    parser = argparse.ArgumentParser(description="Cloud Benchmarker for HugiMQ and Redis")
    parser.add_argument("target", choices=["redis", "hugimq", "hugimqws", "hugimqtcp", "hugimqudp"], help="Benchmark target")
    parser.add_argument("--connections", type=int, default=20, help="Number of connections")
    parser.add_argument("--messages", type=int, default=50000, help="Messages per connection")
    parser.add_argument("--payload", type=int, default=128, help="Payload size in bytes")
    parser.add_argument("--az", type=str, default=None, help="Availability Zone (e.g., us-east-1b)")
    args = parser.parse_args()

    stop_event = threading.Event()

    # Start heartbeat thread
    hb = threading.Thread(target=heartbeat, args=(stop_event,), daemon=True)
    hb.start()

    try:
        cleanup_zombies()
        create_key_pair()
        subnet_id = get_target_subnet(args.az)
        target_id = request_spot_instance(TARGET_TYPE, f'HugiMQ-Target-{args.target}', subnet_id)
        bench_id = request_spot_instance(BENCHMARKER_TYPE, 'HugiMQ-Benchmarker', subnet_id)
        
        ips = {}
        def get_ips(id, key):
            ips[key] = wait_for_instance(id, stop_event)

        t1 = threading.Thread(target=get_ips, args=(target_id, 'target'))
        t2 = threading.Thread(target=get_ips, args=(bench_id, 'bench'))
        t1.start()
        t2.start()
        t1.join()
        t2.join()

        target_pub, target_priv = ips['target']
        bench_pub, bench_priv = ips['bench']
        
        log.info(f"Target Instance (Public: {target_pub}, Private: {target_priv})")
        log.info(f"Benchmarker Instance (Public: {bench_pub}, Private: {bench_priv})")

        common_setup = [
            "export DEBIAN_FRONTEND=noninteractive && sudo -E apt-get update",
            "export DEBIAN_FRONTEND=noninteractive && sudo -E apt-get install -y build-essential curl git pkg-config libssl-dev protobuf-compiler uuid-dev python3-pip libclang-dev libbsd-dev",
            "pip3 install --user cmake",
            "export PATH=$HOME/.local/bin:$PATH && cmake --version",
            "if ! command -v cargo &> /dev/null; then curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y; fi",
            f"source $HOME/.cargo/env && if [ ! -d 'HugiMQ' ]; then git clone {REPO_URL} HugiMQ; else cd HugiMQ && git pull; fi"
        ]

        if args.target == "redis":
            target_setup = [
                "export DEBIAN_FRONTEND=noninteractive && sudo -E apt-get update",
                "export DEBIAN_FRONTEND=noninteractive && sudo -E apt-get install -y redis-server",
                "sudo sed -i 's/bind 127.0.0.1/bind 0.0.0.0/' /etc/redis/redis.conf && " + \
                "sudo sed -i 's/protected-mode yes/protected-mode no/' /etc/redis/redis.conf && " + \
                "sudo sed -i 's/# io-threads 4/io-threads 2/' /etc/redis/redis.conf && " + \
                "sudo sed -i 's/# io-threads-do-reads no/io-threads-do-reads yes/' /etc/redis/redis.conf && " + \
                "sudo systemctl restart redis-server"
            ]
        elif args.target == "hugimqudp":
            # Aeron UDP: needs PATH for pip-installed cmake
            target_setup = common_setup + [
                "export PATH=$HOME/.local/bin:$PATH && source $HOME/.cargo/env && cd HugiMQ && cargo build --release -p hugimq"
            ]
        else:
            target_setup = common_setup + [
                "source $HOME/.cargo/env && cd HugiMQ && cargo build --release -p hugimq"
            ]

        # Aeron benchmarker also needs PATH for cmake
        if args.target == "hugimqudp":
            bench_setup = common_setup + [
                "export PATH=$HOME/.local/bin:$PATH && source $HOME/.cargo/env && cd HugiMQ && cargo build --release -p benchmarker"
            ]
        else:
            bench_setup = common_setup + [
                "source $HOME/.cargo/env && cd HugiMQ && cargo build --release -p benchmarker"
            ]

        log.info("Starting instance setup...")
        t1 = threading.Thread(target=run_ssh_commands, args=(target_pub, target_setup, "TARGET-SETUP", stop_event))
        t2 = threading.Thread(target=run_ssh_commands, args=(bench_pub, bench_setup, "BENCH-SETUP", stop_event))
        t1.start()
        t2.start()
        t1.join()
        t2.join()

        if stop_event.is_set(): return

        if args.target == "hugimq":
            log.info("Starting HugiMQ server on target instance...")
            run_ssh_commands(target_pub, [
                "ls -la $HOME/HugiMQ/target/release/hugimq-server",
                "source $HOME/.cargo/env && (nohup $HOME/HugiMQ/target/release/hugimq-server > $HOME/hugimq.log 2>&1 < /dev/null &)",
                "sleep 10 && (pgrep hugimq-server || (echo '--- LOG START ---' && cat $HOME/hugimq.log && echo '--- LOG END ---' && exit 1))"
            ], "HUGIMQ-START", stop_event)
        elif args.target == "hugimqws":
            log.info("Starting HugiMQ WebSocket server on target instance...")
            run_ssh_commands(target_pub, [
                "ls -la $HOME/HugiMQ/target/release/hugimq-server",
                "source $HOME/.cargo/env && (nohup $HOME/HugiMQ/target/release/hugimq-server > $HOME/hugimq.log 2>&1 < /dev/null &)",
                "sleep 10 && (pgrep hugimq-server || (echo '--- LOG START ---' && cat $HOME/hugimq.log && echo '--- LOG END ---' && exit 1))"
            ], "HUGIMQWS-START", stop_event)
        elif args.target == "hugimqtcp":
            log.info("Starting HugiMQ TCP server on target instance...")
            run_ssh_commands(target_pub, [
                "ls -la $HOME/HugiMQ/target/release/hugimq-server",
                "source $HOME/.cargo/env && (nohup $HOME/HugiMQ/target/release/hugimq-server > $HOME/hugimq.log 2>&1 < /dev/null &)",
                "sleep 10 && (pgrep hugimq-server || (echo '--- LOG START ---' && cat $HOME/hugimq.log && echo '--- LOG END ---' && exit 1))"
            ], "HUGIMQTCP-START", stop_event)
        elif args.target == "hugimqudp":
            log.info("Starting HugiMQ Aeron UDP server on target instance...")
            run_ssh_commands(target_pub, [
                "ls -la $HOME/HugiMQ/target/release/hugimq-server",
                "AERON_LIB=$(find $HOME/HugiMQ/target/release/build -name 'libaeron_driver.so' -printf '%h\n' | head -1) && sudo ldconfig $AERON_LIB && source $HOME/.cargo/env && (nohup $HOME/HugiMQ/target/release/hugimq-server > $HOME/hugimq.log 2>&1 < /dev/null &)",
                "sleep 10 && (pgrep hugimq-server || (echo '--- LOG START ---' && cat $HOME/hugimq.log && echo '--- LOG END ---' && exit 1))"
            ], "HUGIMQUDP-START", stop_event)

        # Run Benchmark
        log.info(f"Running benchmark against {args.target}...")

        if args.target == "redis":
            url_flag = f"--redis-url redis://{target_priv}:6379/"
            bench_target = args.target
        elif args.target == "hugimqws":
            url_flag = f"--hugimq-ws-url ws://{target_priv}:6379"
            bench_target = args.target
        elif args.target == "hugimqtcp":
            url_flag = f"--url tcp://{target_priv}:6379"
            bench_target = "tcp"
        elif args.target == "hugimqudp":
            # Aeron benchmarker uses -s for server IP, no --url or positional target
            url_flag = f"-s {target_priv}"
            bench_target = ""  # No positional arg needed
        else:
            url_flag = f"--hugimq-url http://{target_priv}:6379"
            bench_target = args.target

        if args.target == "hugimqudp":
            benchmark_cmd = [
                f"AERON_LIB=$(find $HOME/HugiMQ/target/release/build -name 'libaeron_driver.so' -printf '%h\n' | head -1) && sudo ldconfig $AERON_LIB && source $HOME/.cargo/env && cd HugiMQ && " + \
                f"./target/release/benchmarker " + \
                f"--connections {args.connections} --messages-per-conn {args.messages} --payload-size {args.payload} " + \
                f"{url_flag}"
            ]
        else:
            benchmark_cmd = [
                f"source $HOME/.cargo/env && cd HugiMQ && ./target/release/benchmarker {bench_target} " + \
                f"--connections {args.connections} --messages-per-conn {args.messages} --payload-size {args.payload} " + \
                f"{url_flag}"
            ]

        run_ssh_commands(bench_pub, benchmark_cmd, "BENCHMARK-RUN", stop_event)
        log.info("Benchmark complete.")

    except Exception as e:
        log.error(f"Error occurred: {e}")
    finally:
        stop_event.set()
        manager.terminate_all()

if __name__ == '__main__':
    main()
