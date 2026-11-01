//! The plan of a Strata cluster on EKS: where its copies go on the nodes,
//! what it costs, what to watch for, the CloudFormation template of the
//! AWS side (a VPC, the cluster, its nodes, the EBS driver) and the
//! StrataCluster the operator keeps on it.
//!
//! Nothing here calls AWS: it works from a region's [`Catalog`].

use serde::Deserialize;
use serde_json::{json, Value as Json};

use super::catalog::Machine;
use super::Catalog;
use crate::cloud::{check_name, cluster_yaml, fit, operator_yaml, Node};
pub use crate::cloud::IMAGE;

/// Hours in a month, as AWS prices them.
pub const HOURS_A_MONTH: f64 = 730.0;

/// Each node's root disk (EKS's default), GiB.
const ROOT_GB: f64 = 20.0;

/// The pods every node runs besides Strata's: aws-node, kube-proxy, the
/// EBS driver's node pod and the pod identity agent.
const NODE_PODS: u32 = 4;

/// The cluster's own pods: two CoreDNS and two EBS controllers.
const SYSTEM_PODS: u32 = 4;

/// What is asked for: `POST /v1/admin/aws/plan`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct Plan {
    /// The cluster's name: the EKS cluster and the stack are
    /// `strata-<name>`.
    pub name: String,
    pub region: String,
    /// The nodes' instance type.
    pub machine: String,
    pub nodes: u32,
    /// Spot nodes in place of on-demand ones.
    pub spot: bool,
    /// Each copy's volume (gp3), GiB.
    pub volume_gb: u32,
    /// The StrataCluster's spec, in part: what is left out is filled in
    /// (`shards`, `replicas`, `coordinators`, `demo` 10,000, the image,
    /// the volume's size and class, each copy's share of a node).
    pub cluster: Json,
}

impl Default for Plan {
    fn default() -> Plan {
        Plan { name: "search".into(), region: "us-east-1".into(), machine: "m8g.large".into(), nodes: 3, spot: false, volume_gb: 10, cluster: json!({}) }
    }
}

impl Plan {
    pub fn stack(&self) -> String {
        format!("strata-{}", self.name)
    }
}

/// The memory a node's pods may have, GiB: what EKS keeps for the kubelet
/// (255 MiB and 11 MiB a pod it may run) and for eviction (100 MiB), less
/// half a GiB for the node's own pods.
pub fn allocatable_gib(m: &Machine) -> f64 {
    (m.memory_gib - (255.0 + 11.0 * f64::from(m.max_pods) + 100.0) / 1024.0 - 0.5).max(0.0)
}

/// The plan: `Err` when it cannot be made.
pub fn plan(p: &Plan, cat: &Catalog, principal_arn: &str) -> Result<Json, String> {
    check_name(&p.name)?;
    if p.region != cat.region {
        return Err(format!("the machines are those of {}, not {}", cat.region, p.region));
    }
    let m = cat.machines.iter().find(|m| m.name == p.machine).ok_or_else(|| format!("{} is not a machine a cluster runs on in {} (the current generation, on demand, without accelerators)", p.machine, p.region))?;
    if !(1..=100).contains(&p.nodes) {
        return Err("nodes is 1 to 100".into());
    }
    if !(1..=16_384).contains(&p.volume_gb) {
        return Err("volume_gb is 1 to 16,384 (a gp3 volume's size)".into());
    }
    let alloc = allocatable_gib(m);
    let f = fit(&p.cluster, p.nodes, &Node { vcpu: m.vcpu, alloc_gib: alloc }, p.volume_gb, "gp3")?;
    let (per_node, coordinators, replicas) = (f.per_node, f.coordinators, f.replicas);

    let mut warnings = f.warnings(p.nodes);
    // The quota, in the vCPUs of standard instances.
    let need = p.nodes * m.vcpu;
    let (quota, kind) = if p.spot { (cat.spot_quota_vcpus, "spot") } else { (cat.quota_vcpus, "on-demand") };
    match quota {
        Some(q) if f64::from(need) > q => warnings.push(format!(
            "the account may run {q} vCPUs of standard {kind} instances in {}, and {} × {} need {need}: the nodes would not start. Ask AWS for more in Service Quotas (EC2, \"{}\")",
            p.region,
            p.nodes,
            m.name,
            if p.spot { "All Standard (A, C, D, H, I, M, R, T, Z) Spot Instance Requests" } else { "Running On-Demand Standard (A, C, D, H, I, M, R, T, Z) instances" }
        )),
        None => warnings.push(format!("the account's {kind} vCPU quota in {} is not known", p.region)),
        _ => {}
    }
    let pods = per_node + NODE_PODS + (SYSTEM_PODS + coordinators + 1).div_ceil(p.nodes);
    if pods > m.max_pods {
        warnings.push(format!("a node would run {pods} pods, and EKS lets a {} run {}: some copies would not start. Fewer copies a node, or a larger machine", m.name, m.max_pods));
    }
    if m.burstable {
        warnings.push(format!("{} is burstable: under steady queries its CPU credits run out and it slows to its baseline", m.name));
    }
    if m.local_gb > 0 {
        warnings.push(format!("{} has {} GB of local disk the cluster does not use: the copies keep their data on EBS volumes", m.name, m.local_gb));
    }
    if p.spot {
        warnings.push("spot nodes may be taken back with two minutes' notice: copies fail over, but timed benchmarks should run on on-demand nodes".into());
    }
    if m.usd_hour.is_none() {
        warnings.push(format!("{} has no on-demand price in the price list here", m.name));
    }

    let cost = cost(p, m, cat, f.shards * replicas);
    let image = f.cluster["image"].as_str().unwrap_or(IMAGE).to_string();
    let stack = p.stack();
    let r = &p.region;
    let commands = [
        format!("aws cloudformation create-stack --region {r} --stack-name {stack} --template-body file://{stack}.json --capabilities CAPABILITY_IAM"),
        format!("aws cloudformation wait stack-create-complete --region {r} --stack-name {stack}"),
        format!("aws eks update-kubeconfig --region {r} --name {stack}"),
        format!("kubectl apply -f {stack}-operator.yaml"),
        "kubectl wait --for condition=established --timeout=180s crd/strataclusters.strata.chloetech.ai".to_string(),
        format!("kubectl apply -f {stack}-cluster.yaml"),
        format!("kubectl -n strata get strata {} -w", p.name),
    ];
    // The copies' volumes are the EBS driver's, not the stack's: deleted
    // with their claims, before the stack, or they stay and cost.
    let teardown = [
        format!("kubectl delete -f {stack}-cluster.yaml"),
        "kubectl -n strata delete pvc --all --wait".to_string(),
        format!("aws cloudformation delete-stack --region {r} --stack-name {stack}"),
        format!("aws cloudformation wait stack-delete-complete --region {r} --stack-name {stack}"),
    ];
    Ok(json!({
        "provider": "aws",
        "stack": p.stack(),
        "cluster_name": p.stack(),
        "region": p.region,
        "machine": m,
        "layout": f.layout_json(pods, m.max_pods),
        "quota": {"vcpus": quota, "needs": need, "kind": kind},
        "cost": cost,
        "warnings": warnings,
        "manifests": {"operator": operator_yaml(&image)?, "cluster": cluster_yaml("A Strata cluster on EKS, from strata-server's cloud view", &p.name, &f.cluster, Some(gp3_class()))},
        "commands": commands,
        "teardown": teardown,
        "cluster": f.cluster,
        "template": template(p, m, principal_arn),
    }))
}

/// A gp3 storage class that grows volumes in place: EKS has none.
fn gp3_class() -> Json {
    json!({"apiVersion": "storage.k8s.io/v1", "kind": "StorageClass", "metadata": {"name": "gp3"}, "provisioner": "ebs.csi.aws.com",
        "parameters": {"type": "gp3", "encrypted": "true"}, "volumeBindingMode": "WaitForFirstConsumer", "allowVolumeExpansion": true, "reclaimPolicy": "Delete"})
}

/// What the plan costs, an hour and a month, line by line: the nodes, the
/// EKS cluster, the volumes, the nodes' public addresses. Lines whose
/// price is not known say so and are left out of the sum.
pub fn cost(p: &Plan, m: &Machine, cat: &Catalog, copies: u32) -> Json {
    let n = f64::from(p.nodes);
    let node = if p.spot { m.spot_hour } else { m.usd_hour };
    let gb = f64::from(copies) * f64::from(p.volume_gb) + n * ROOT_GB;
    let lines = [
        (format!("{} × {} ({})", p.nodes, m.name, if p.spot { "spot, latest price" } else { "on demand" }), node.map(|h| h * n)),
        ("the EKS cluster".to_string(), cat.eks_hour),
        (format!("{gb} GiB of gp3 volumes ({copies} copies × {} GiB, {} roots × {ROOT_GB} GiB)", p.volume_gb, p.nodes), cat.gp3_gb_month.map(|g| g * gb / HOURS_A_MONTH)),
        (format!("{} public IPv4 addresses", p.nodes), cat.ipv4_hour.map(|h| h * n)),
    ];
    let hour: f64 = lines.iter().filter_map(|(_, h)| *h).sum();
    let unknown: Vec<&str> = lines.iter().filter(|(_, h)| h.is_none()).map(|(w, _)| w.as_str()).collect();
    let on_demand = m.usd_hour.map(|h| h * n);
    let spot = m.spot_hour.map(|h| h * n);
    json!({
        "hour": hour,
        "month": hour * HOURS_A_MONTH,
        "lines": lines.iter().map(|(w, h)| json!({"what": w, "hour": h, "month": h.map(|h| h * HOURS_A_MONTH)})).collect::<Vec<_>>(),
        "unknown": unknown,
        "nodes": {"on_demand_hour": on_demand, "spot_hour": spot},
        "not_counted": "data sent between zones and out of AWS, and logs",
    })
}

/// The CloudFormation template of the AWS side: a VPC over three zones
/// with public subnets (no NAT gateway to pay for), the EKS cluster with
/// `principal_arn` as its administrator, a managed node group, and the
/// EBS CSI driver with its role (EKS Pod Identity).
pub fn template(p: &Plan, m: &Machine, principal_arn: &str) -> Json {
    let name = p.stack();
    let arm = m.arch == "arm64";
    let tags = |n: &str| json!([{"Key": "Name", "Value": format!("{name}-{n}")}, {"Key": "strata.chloetech.ai/cluster", "Value": name}]);
    let policy = |p: &str| json!({"Fn::Sub": format!("arn:${{AWS::Partition}}:iam::aws:policy/{p}")});
    let assume = |service: &str, actions: &[&str]| json!({"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Principal": {"Service": service}, "Action": actions}]});
    let mut r = serde_json::Map::new();
    r.insert("Vpc".into(), json!({"Type": "AWS::EC2::VPC", "Properties": {"CidrBlock": "10.42.0.0/16", "EnableDnsSupport": true, "EnableDnsHostnames": true, "Tags": tags("vpc")}}));
    r.insert("Gateway".into(), json!({"Type": "AWS::EC2::InternetGateway", "Properties": {"Tags": tags("gateway")}}));
    r.insert("GatewayAttachment".into(), json!({"Type": "AWS::EC2::VPCGatewayAttachment", "Properties": {"VpcId": {"Ref": "Vpc"}, "InternetGatewayId": {"Ref": "Gateway"}}}));
    r.insert("Routes".into(), json!({"Type": "AWS::EC2::RouteTable", "Properties": {"VpcId": {"Ref": "Vpc"}, "Tags": tags("routes")}}));
    r.insert("DefaultRoute".into(), json!({"Type": "AWS::EC2::Route", "DependsOn": "GatewayAttachment", "Properties": {"RouteTableId": {"Ref": "Routes"}, "DestinationCidrBlock": "0.0.0.0/0", "GatewayId": {"Ref": "Gateway"}}}));
    let mut subnets = Vec::new();
    for z in 0..3 {
        let s = format!("Subnet{z}");
        r.insert(s.clone(), json!({"Type": "AWS::EC2::Subnet", "Properties": {
            "VpcId": {"Ref": "Vpc"},
            "CidrBlock": format!("10.42.{}.0/18", z * 64),
            "AvailabilityZone": {"Fn::Select": [z, {"Fn::GetAZs": ""}]},
            "MapPublicIpOnLaunch": true,
            "Tags": [{"Key": "Name", "Value": format!("{name}-zone-{z}")}, {"Key": "kubernetes.io/role/elb", "Value": "1"}, {"Key": "strata.chloetech.ai/cluster", "Value": name}],
        }}));
        r.insert(format!("Subnet{z}Routes"), json!({"Type": "AWS::EC2::SubnetRouteTableAssociation", "Properties": {"SubnetId": {"Ref": s}, "RouteTableId": {"Ref": "Routes"}}}));
        subnets.push(json!({"Ref": s}));
    }
    r.insert("ClusterRole".into(), json!({"Type": "AWS::IAM::Role", "Properties": {"AssumeRolePolicyDocument": assume("eks.amazonaws.com", &["sts:AssumeRole", "sts:TagSession"]), "ManagedPolicyArns": [policy("AmazonEKSClusterPolicy")]}}));
    r.insert("NodeRole".into(), json!({"Type": "AWS::IAM::Role", "Properties": {"AssumeRolePolicyDocument": assume("ec2.amazonaws.com", &["sts:AssumeRole"]), "ManagedPolicyArns": [policy("AmazonEKSWorkerNodePolicy"), policy("AmazonEKS_CNI_Policy"), policy("AmazonEC2ContainerRegistryReadOnly")]}}));
    r.insert("VolumeRole".into(), json!({"Type": "AWS::IAM::Role", "Properties": {"AssumeRolePolicyDocument": assume("pods.eks.amazonaws.com", &["sts:AssumeRole", "sts:TagSession"]), "ManagedPolicyArns": [policy("service-role/AmazonEBSCSIDriverPolicy")]}}));
    r.insert("Cluster".into(), json!({"Type": "AWS::EKS::Cluster", "Properties": {
        "Name": name,
        "RoleArn": {"Fn::GetAtt": ["ClusterRole", "Arn"]},
        "ResourcesVpcConfig": {"SubnetIds": subnets, "EndpointPublicAccess": true, "EndpointPrivateAccess": true},
        "AccessConfig": {"AuthenticationMode": "API", "BootstrapClusterCreatorAdminPermissions": false},
        "Tags": [{"Key": "strata.chloetech.ai/cluster", "Value": name}],
    }}));
    r.insert("Administrator".into(), json!({"Type": "AWS::EKS::AccessEntry", "Properties": {
        "ClusterName": {"Ref": "Cluster"},
        "PrincipalArn": principal_arn,
        "AccessPolicies": [{"PolicyArn": {"Fn::Sub": "arn:${AWS::Partition}:eks::aws:cluster-access-policy/AmazonEKSClusterAdminPolicy"}, "AccessScope": {"Type": "cluster"}}],
    }}));
    r.insert("Nodes".into(), json!({"Type": "AWS::EKS::Nodegroup", "Properties": {
        "ClusterName": {"Ref": "Cluster"},
        "NodegroupName": "copies",
        "NodeRole": {"Fn::GetAtt": ["NodeRole", "Arn"]},
        "Subnets": (0..3).map(|z| json!({"Ref": format!("Subnet{z}")})).collect::<Vec<_>>(),
        "InstanceTypes": [m.name],
        "AmiType": if arm { "AL2023_ARM_64_STANDARD" } else { "AL2023_x86_64_STANDARD" },
        "CapacityType": if p.spot { "SPOT" } else { "ON_DEMAND" },
        "DiskSize": ROOT_GB as u32,
        "ScalingConfig": {"MinSize": p.nodes, "DesiredSize": p.nodes, "MaxSize": p.nodes},
        "Labels": {"strata.chloetech.ai/pool": "copies"},
        "Tags": {"strata.chloetech.ai/cluster": name},
    }}));
    r.insert("PodIdentity".into(), json!({"Type": "AWS::EKS::Addon", "DependsOn": "Nodes", "Properties": {"ClusterName": {"Ref": "Cluster"}, "AddonName": "eks-pod-identity-agent", "ResolveConflicts": "OVERWRITE"}}));
    r.insert("Volumes".into(), json!({"Type": "AWS::EKS::Addon", "DependsOn": ["Nodes", "PodIdentity"], "Properties": {
        "ClusterName": {"Ref": "Cluster"},
        "AddonName": "aws-ebs-csi-driver",
        "ResolveConflicts": "OVERWRITE",
        "PodIdentityAssociations": [{"ServiceAccount": "ebs-csi-controller-sa", "RoleArn": {"Fn::GetAtt": ["VolumeRole", "Arn"]}}],
    }}));
    json!({
        "AWSTemplateFormatVersion": "2010-09-09",
        "Description": format!("Strata cluster {name}: EKS with {} {} nodes ({}), made by strata-server's cloud view", p.nodes, m.name, if p.spot { "spot" } else { "on demand" }),
        "Resources": r,
        "Outputs": {
            "ClusterName": {"Value": {"Ref": "Cluster"}},
            "Endpoint": {"Value": {"Fn::GetAtt": ["Cluster", "Endpoint"]}},
            "CertificateAuthority": {"Value": {"Fn::GetAtt": ["Cluster", "CertificateAuthorityData"]}},
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(name: &str, vcpu: u32, mem: f64, usd: f64, spot: f64, pods: u32) -> Machine {
        Machine { name: name.into(), family: name.split('.').next().unwrap().into(), kind: "general", arch: "arm64".into(), vcpu, memory_gib: mem, burstable: false, local_gb: 0, network: String::new(), ebs_mbps: 0, max_pods: pods, processor: None, usd_hour: Some(usd), spot_hour: Some(spot) }
    }

    fn catalog(quota: f64) -> Catalog {
        Catalog {
            region: "us-east-1".into(),
            at_ms: 0,
            machines: vec![machine("m8g.large", 2, 8.0, 0.08976, 0.0381, 29), machine("r8g.xlarge", 4, 32.0, 0.23564, 0.0733, 58), machine("m8g.medium", 1, 4.0, 0.04488, 0.02, 8)],
            quota_vcpus: Some(quota),
            spot_quota_vcpus: Some(0.0),
            eks_hour: Some(0.10),
            gp3_gb_month: Some(0.08),
            ipv4_hour: Some(0.005),
            missing: vec![],
        }
    }

    #[test]
    fn a_plan_costs_what_its_parts_cost_and_says_what_stops_it() {
        let p = Plan::default();
        let j = plan(&p, &catalog(32.0), "arn:aws:iam::123456789012:user/dev").unwrap();
        // 3 m8g.large: 0.26928, EKS 0.10, (6 × 10 + 3 × 20) GiB gp3, 3 addresses.
        let hour = 3.0 * 0.08976 + 0.10 + 120.0 * 0.08 / 730.0 + 3.0 * 0.005;
        assert!((j["cost"]["hour"].as_f64().unwrap() - hour).abs() < 1e-9, "{}", j["cost"]);
        assert!((j["cost"]["month"].as_f64().unwrap() - hour * 730.0).abs() < 1e-6);
        assert_eq!(j["cost"]["lines"][0]["what"], "3 × m8g.large (on demand)");
        assert_eq!(j["warnings"], json!([]), "{}", j["warnings"]);
        assert_eq!((j["layout"]["copies_per_node"].as_u64(), j["quota"]["needs"].as_u64()), (Some(2), Some(6)));
        // The StrataCluster: what was left out, filled in.
        let c = &j["cluster"];
        assert_eq!((&c["shards"], &c["replicas"], &c["image"], &c["storage"]), (&json!(2), &json!(3), &json!(IMAGE), &json!({"size": "10Gi", "storageClassName": "gp3"})));
        assert!(c["resources"]["requests"]["memory"].as_str().unwrap().ends_with("Mi"));
        let op = j["manifests"]["operator"].as_str().unwrap();
        assert!(op.contains(&format!("image: {IMAGE}")) && !op.contains("strata-server:dev"), "{op}");
        let cl = j["manifests"]["cluster"].as_str().unwrap();
        assert_eq!(cl.matches("---\n").count(), 3);
        for d in cl.split("---\n").skip(1) {
            let d: Json = serde_json::from_str(d).unwrap();
            assert!(d["kind"].is_string());
        }
        assert!(j["commands"][0].as_str().unwrap().starts_with("aws cloudformation create-stack --region us-east-1 --stack-name strata-search --template-body file://strata-search.json --capabilities CAPABILITY_IAM"));
        assert_eq!(j["teardown"][1], "kubectl -n strata delete pvc --all --wait");

        // The template: the node group as asked, the caller its admin.
        let t = &j["template"]["Resources"];
        assert_eq!(t["Nodes"]["Properties"]["InstanceTypes"], json!(["m8g.large"]));
        assert_eq!(t["Nodes"]["Properties"]["AmiType"], "AL2023_ARM_64_STANDARD");
        assert_eq!(t["Nodes"]["Properties"]["ScalingConfig"]["DesiredSize"], 3);
        assert_eq!(t["Administrator"]["Properties"]["PrincipalArn"], "arn:aws:iam::123456789012:user/dev");
        assert_eq!(t["Volumes"]["Properties"]["PodIdentityAssociations"][0]["ServiceAccount"], "ebs-csi-controller-sa");
        assert_eq!(t.as_object().unwrap().len(), 19);
        assert!(j["template"].to_string().len() < 51_200, "a template body CloudFormation takes inline");

        // A quota of 0, spot, too few nodes, too small a machine.
        let p = Plan { machine: "m8g.medium".into(), nodes: 2, spot: true, ..Plan::default() };
        let j = plan(&p, &catalog(0.0), "arn").unwrap();
        let w = j["warnings"].to_string();
        for s in ["may run 0 vCPUs of standard spot instances in us-east-1", "a shard has 3 copies and there are 2 nodes", "spot nodes may be taken back", "EKS lets a m8g.medium run 8"] {
            assert!(w.contains(s), "{s} in {w}");
        }
        assert_eq!(j["cost"]["lines"][0]["what"], "2 × m8g.medium (spot, latest price)");
        assert_eq!(j["template"]["Resources"]["Nodes"]["Properties"]["CapacityType"], "SPOT");

        // What cannot be planned.
        let bad = |p: Plan| plan(&p, &catalog(32.0), "arn").unwrap_err();
        assert!(bad(Plan { name: "Search!".into(), ..Plan::default() }).contains("not a cluster name"));
        assert!(bad(Plan { machine: "p5.48xlarge".into(), ..Plan::default() }).contains("not a machine a cluster runs on"));
        assert!(bad(Plan { nodes: 0, ..Plan::default() }).contains("nodes is 1 to 100"));
        assert!(bad(Plan { region: "eu-west-1".into(), ..Plan::default() }).contains("those of us-east-1"));
        assert!(bad(Plan { cluster: json!({"replicas": 0}), ..Plan::default() }).contains("cluster: shards, replicas and coordinators are 1 or more"));
    }
}
