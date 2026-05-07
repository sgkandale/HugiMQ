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

load_dotenv()

AWS_ACCESS_KEY = os.getenv('AWS_ACCESS_KEY')
AWS_SECRET_KEY = os.getenv('AWS_SECRET_KEY')
AWS_REGION = os.getenv('AWS_REGION', 'us-east-1')
AMI_ID = os.getenv('AMI_ID', 'ami-0c7217cdde317cfec')
KEY_NAME = os.getenv('KEY_NAME', 'hugimq-bench-key')
KEY_PATH = os.getenv('KEY_PATH', './hugimq-bench-key.pem')

if not AWS_ACCESS_KEY or not AWS_SECRET_KEY:
    print("Error: AWS_ACCESS_KEY and AWS_SECRET_KEY must be set in your .env file.")
    sys.exit(1)

AMI_ID = 'ami-0ab515046786de9dc' # Ubuntu 22.04 ARM64
BENCHMARKER_TYPE = 'hpc7g.8xlarge'
TARGET_TYPE = 'hpc7g.4xlarge'
SECURITY_GROUP_NAME = 'hugimq-bench-sg'
PLACEMENT_GROUP_NAME = 'hugimq-cluster-pg'
REPO_URL = os.getenv('REPO_URL', 'https://github.com/sgkandale/HugiMQ.git')
SERVER_PORT = 6379

logging.basicConfig(
    level=logging.INFO,
    format='%(asctime)s - %(levelname)s - %(message)s',
    datefmt='%H:%M:%S'
)
log = logging.getLogger("cloud_bench")

def heartbeat(stop_event):
    while not stop_event.is_set():
        time.sleep(30)
        if not stop_event.is_set():
            sys.stdout.write(".")
            sys.stdout.flush()

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
                ec2.terminate_instances(InstanceIds=self.instances)
                self.instances = []
            except Exception as e:
                log.error(f"Failed to terminate instances: {e}")

manager = InstanceManager()

def cleanup_zombies():
    try:
        res = ec2.describe_instances(Filters=[
            {'Name': 'tag:Name', 'Values': ['HugiMQ-*']},
            {'Name': 'instance-state-name', 'Values': ['running', 'pending']}
        ])
        ids = [i['InstanceId'] for r in res['Reservations'] for i in r['Instances']]
        if ids:
            log.warning(f"Terminating zombies: {ids}")
            ec2.terminate_instances(InstanceIds=ids)
    except: pass

def signal_handler(signum, frame):
    manager.terminate_all()
    sys.exit(1)

signal.signal(signal.SIGINT, signal_handler)

def create_key_pair():
    try: ec2.describe_key_pairs(KeyNames=[KEY_NAME])
    except ClientError:
        log.info(f"Creating key pair: {KEY_NAME}")
        response = ec2.create_key_pair(KeyName=KEY_NAME)
        with open(KEY_PATH, 'w') as f: f.write(response['KeyMaterial'])
        os.chmod(KEY_PATH, 0o400)

def create_security_group():
    try:
        response = ec2.create_security_group(GroupName=SECURITY_GROUP_NAME, Description='HugiMQ SG')
        sg_id = response['GroupId']
        ec2.authorize_security_group_ingress(
            GroupId=sg_id,
            IpPermissions=[
                {'IpProtocol': 'tcp', 'FromPort': 22, 'ToPort': 22, 'IpRanges': [{'CidrIp': '0.0.0.0/0'}]},
                {'IpProtocol': 'tcp', 'FromPort': SERVER_PORT, 'ToPort': SERVER_PORT, 'IpRanges': [{'CidrIp': '0.0.0.0/0'}]},
            ]
        )
        return sg_id
    except ClientError:
        res = ec2.describe_security_groups(GroupNames=[SECURITY_GROUP_NAME])
        return res['SecurityGroups'][0]['GroupId']

def get_target_subnet(az=None):
    if az:
        res = ec2.describe_subnets(Filters=[{'Name': 'availability-zone', 'Values': [az]}])
    else:
        res = ec2.describe_subnets()
    if not res['Subnets']:
        raise Exception(f"No subnets found in AZ {az}" if az else "No subnets found")
    return res['Subnets'][0]['SubnetId']

def request_spot_instance(instance_type, name, subnet_id, sg_id):
    log.info(f"Requesting Spot instance: {instance_type} ({name})")
    res = ec2.run_instances(
        ImageId=AMI_ID, 
        InstanceType=instance_type, 
        KeyName=KEY_NAME,
        NetworkInterfaces=[{
            'DeviceIndex': 0,
            'SubnetId': subnet_id,
            'Groups': [sg_id],
            'AssociatePublicIpAddress': True
        }],
        Placement={'GroupName': PLACEMENT_GROUP_NAME},
        InstanceMarketOptions={'MarketType': 'spot'}, 
        MinCount=1, 
        MaxCount=1,
        TagSpecifications=[{'ResourceType': 'instance', 'Tags': [{'Key': 'Name', 'Value': name}]}]
    )
    instance_id = res['Instances'][0]['InstanceId']
    manager.add(instance_id)
    return instance_id

def request_ondemand_instance(instance_type, name, subnet_id, sg_id):
    log.info(f"Requesting On-Demand instance: {instance_type} ({name})")
    res = ec2.run_instances(
        ImageId=AMI_ID, 
        InstanceType=instance_type, 
        KeyName=KEY_NAME,
        NetworkInterfaces=[{
            'DeviceIndex': 0,
            'SubnetId': subnet_id,
            'Groups': [sg_id],
            'AssociatePublicIpAddress': True
        }],
        Placement={'GroupName': PLACEMENT_GROUP_NAME},
        MinCount=1, 
        MaxCount=1,
        TagSpecifications=[{'ResourceType': 'instance', 'Tags': [{'Key': 'Name', 'Value': name}]}]
    )
    instance_id = res['Instances'][0]['InstanceId']
    manager.add(instance_id)
    return instance_id

def wait_for_instance(instance_id):
    log.info(f"Waiting for {instance_id}...")
    waiter = ec2.get_waiter('instance_status_ok')
    waiter.wait(InstanceIds=[instance_id])
    res = ec2.describe_instances(InstanceIds=[instance_id])
    instance = res['Reservations'][0]['Instances'][0]
    return instance['PublicIpAddress'], instance['PrivateIpAddress']

def run_ssh_commands(ip, commands, prefix, stop_event):
    client = paramiko.SSHClient()
    client.set_missing_host_key_policy(paramiko.AutoAddPolicy())
    for i in range(10):
        try:
            client.connect(hostname=ip, username='ubuntu', key_filename=KEY_PATH, timeout=10)
            break
        except: time.sleep(10)
    
    for cmd in commands:
        if stop_event.is_set(): break
        stdin, stdout, stderr = client.exec_command(cmd)
        for line in stdout: log.info(f"[{prefix}] {line.strip()}")
        for line in stderr: log.warning(f"[{prefix}] [ERR] {line.strip()}")
    client.close()

def create_placement_group():
    try:
        log.info(f"Creating placement group: {PLACEMENT_GROUP_NAME}")
        ec2.create_placement_group(GroupName=PLACEMENT_GROUP_NAME, Strategy='cluster')
    except ClientError as e:
        if e.response['Error']['Code'] != 'InvalidPlacementGroup.Duplicate':
            log.error(f"Failed to create placement group: {e}")

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("target", choices=["tcp", "redis"], default="tcp")
    parser.add_argument("--connections", type=int, default=20)
    parser.add_argument("--topics", type=int, default=1)
    parser.add_argument("--messages-per-conn", type=int, default=100000)
    parser.add_argument("--payload-size", type=int, default=128)
    parser.add_argument("--az", type=str, help="AWS Availability Zone")
    parser.add_argument("--variable-payload", action="store_true", help="Enable variable payload size (512B-10KB)")
    args = parser.parse_args()

    stop_event = threading.Event()
    threading.Thread(target=heartbeat, args=(stop_event,), daemon=True).start()

    try:
        cleanup_zombies()
        create_key_pair()
        create_placement_group()
        sg_id = create_security_group()
        sub_id = get_target_subnet(args.az)
        
        # server_id = request_spot_instance(TARGET_TYPE, 'HugiMQ-Server', sub_id, sg_id)
        # bench_id = request_spot_instance(BENCHMARKER_TYPE, 'HugiMQ-Bench', sub_id, sg_id)
        
        server_id = request_ondemand_instance(TARGET_TYPE, 'HugiMQ-Server', sub_id, sg_id)
        bench_id = request_ondemand_instance(BENCHMARKER_TYPE, 'HugiMQ-Bench', sub_id, sg_id)
        
        server_pub, server_priv = wait_for_instance(server_id)
        bench_pub, bench_priv = wait_for_instance(bench_id)

        log.info(f"Server: {server_pub} ({server_priv})")
        log.info(f"Benchmarker: {bench_pub} ({bench_priv})")

        setup_cmds = [
            "sudo apt-get update && sudo apt-get install -y build-essential git curl pkg-config libssl-dev",
            "if ! command -v cargo &> /dev/null; then curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y; fi",
            f"rm -rf HugiMQ && git clone {REPO_URL} HugiMQ && cd HugiMQ && source $HOME/.cargo/env && cargo build --release"
        ]

        if args.target == "redis":
            redis_setup = [
                "sudo DEBIAN_FRONTEND=noninteractive apt-get update",
                "sudo DEBIAN_FRONTEND=noninteractive apt-get install -y redis-server",
                "echo 'bind 0.0.0.0' | sudo tee /etc/redis/redis.conf",
                "echo 'protected-mode no' | sudo tee -a /etc/redis/redis.conf",
                "echo 'port 6379' | sudo tee -a /etc/redis/redis.conf",
                "sudo systemctl restart redis-server",
                "sleep 2",
                "ss -ln | grep 6379"
            ]
            t1 = threading.Thread(target=run_ssh_commands, args=(server_pub, redis_setup, "REDIS-SETUP", stop_event))
        else:
            t1 = threading.Thread(target=run_ssh_commands, args=(server_pub, setup_cmds, "SERVER-SETUP", stop_event))
            
        t2 = threading.Thread(target=run_ssh_commands, args=(bench_pub, setup_cmds, "BENCH-SETUP", stop_event))
        t1.start(); t2.start(); t1.join(); t2.join()

        server_url = f"tcp://{server_priv}:{SERVER_PORT}"
        if args.target == "redis":
            server_url = f"redis://{server_priv}:{SERVER_PORT}"
        
        if args.target == "tcp":
            server_run = [
                f"source $HOME/.cargo/env && cd HugiMQ && (nohup ./target/release/hugimq-server > server.log 2>&1 &)",
                "sleep 2"
            ]
            run_ssh_commands(server_pub, server_run, "SERVER-RUN", stop_event)

        var_flag = "--variable-payload" if args.variable_payload else ""
        bench_run = [
            f"source $HOME/.cargo/env && cd HugiMQ && ./target/release/benchmarker {args.target} --url {server_url} --connections {args.connections} --topics {args.topics} --messages-per-conn {args.messages_per_conn} --payload-size {args.payload_size} {var_flag}"
        ]
        run_ssh_commands(bench_pub, bench_run, "BENCH-RUN", stop_event)

    finally:
        stop_event.set()
        manager.terminate_all()

if __name__ == '__main__': main()