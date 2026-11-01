//! The machines of a region: EC2's instance types, with their on-demand
//! prices from the AWS Price List API and their latest spot prices.
//!
//! Only machines a Strata cluster may run on are kept: the current
//! generation, sold on demand, not bare metal, and without GPUs or other
//! accelerators (no `p`, `g`, `inf`, `trn`, `dl`, `f`, `vt`, nor the
//! `u-` high-memory, `mac` and `hpc` families).

use std::collections::HashMap;

use serde::Serialize;
use serde_json::Value as Json;

/// One instance type.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Machine {
    /// `m8g.large`.
    #[serde(rename = "type")]
    pub name: String,
    /// `m8g`.
    pub family: String,
    /// What it is for: general, compute, memory, storage or burstable.
    pub kind: &'static str,
    /// `arm64` (AWS Graviton) or `x86_64`.
    pub arch: String,
    pub vcpu: u32,
    pub memory_gib: f64,
    /// Bursts on CPU credits (`t` families).
    pub burstable: bool,
    /// Local NVMe or SSD, in GB (0: EBS only).
    pub local_gb: u32,
    pub network: String,
    /// Baseline EBS bandwidth, Mbit/s.
    pub ebs_mbps: u32,
    /// How many pods EKS's VPC CNI lets a node of this type run.
    pub max_pods: u32,
    /// From the price list: "AWS Graviton4 Processor".
    pub processor: Option<String>,
    /// On-demand Linux price, US dollars an hour (`None`: not priced).
    pub usd_hour: Option<f64>,
    /// The lowest latest spot price over the region's zones.
    pub spot_hour: Option<f64>,
}

/// What a family is for, by the letters its name starts with; `None` for
/// families a cluster does not run on.
pub fn kind(family: &str) -> Option<&'static str> {
    let series: String = family.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    if family.starts_with("u-") || family.starts_with("u7") {
        return None;
    }
    Some(match series.as_str() {
        "t" => "burstable",
        "m" | "a" => "general",
        "c" => "compute",
        "r" | "x" | "z" => "memory",
        "i" | "im" | "is" | "d" | "h" => "storage",
        _ => return None,
    })
}

/// The pods EKS's VPC CNI lets a node run: each network interface's
/// addresses but its own, and two that use the node's (kube-proxy and
/// aws-node); at most 110 under 30 vCPUs and 250 from there, as EKS caps
/// them.
pub fn max_pods(enis: u32, ips_per_eni: u32, vcpu: u32) -> u32 {
    let n = enis * ips_per_eni.saturating_sub(1) + 2;
    n.min(if vcpu < 30 { 110 } else { 250 })
}

fn child<'a, 'i>(n: roxmltree::Node<'a, 'i>, name: &str) -> Option<roxmltree::Node<'a, 'i>> {
    n.children().find(|c| c.is_element() && c.tag_name().name() == name)
}

fn path<'a, 'i>(n: roxmltree::Node<'a, 'i>, names: &[&str]) -> Option<roxmltree::Node<'a, 'i>> {
    names.iter().try_fold(n, |n, name| child(n, name))
}

fn text(n: roxmltree::Node, names: &[&str]) -> Option<String> {
    path(n, names).and_then(|n| n.text()).map(|t| t.trim().to_string())
}

fn items<'a, 'i>(n: roxmltree::Node<'a, 'i>, names: &[&str]) -> Vec<roxmltree::Node<'a, 'i>> {
    path(n, names).map(|s| s.children().filter(|c| c.is_element() && c.tag_name().name() == "item").collect()).unwrap_or_default()
}

/// An EC2 answer, or the error it holds (`<Response><Errors><Error>`).
pub fn parse(xml: &str) -> Result<roxmltree::Document<'_>, String> {
    let d = roxmltree::Document::parse(xml).map_err(|e| format!("an answer that is not XML: {e}"))?;
    if let Some(e) = d.descendants().find(|n| n.has_tag_name("Error")) {
        let code = text(e, &["Code"]).unwrap_or_default();
        return Err(format!("{code}: {}", text(e, &["Message"]).unwrap_or_default()));
    }
    Ok(d)
}

/// A page of `DescribeInstanceTypes`: the machines a cluster may run on,
/// and the token of the next page.
pub fn instance_types(xml: &str) -> Result<(Vec<Machine>, Option<String>), String> {
    let d = parse(xml)?;
    let root = d.root_element();
    let mut out = Vec::new();
    for it in items(root, &["instanceTypeSet"]) {
        let t = |p: &[&str]| text(it, p);
        let num = |p: &[&str]| t(p).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
        let Some(name) = t(&["instanceType"]) else { continue };
        let family = name.split('.').next().unwrap_or_default().to_string();
        let Some(kind) = kind(&family) else { continue };
        let on_demand = items(it, &["supportedUsageClasses"]).iter().any(|i| i.text() == Some("on-demand"));
        if t(&["currentGeneration"]).as_deref() != Some("true") || t(&["bareMetal"]).as_deref() == Some("true") || !on_demand {
            continue;
        }
        let archs: Vec<String> = items(it, &["processorInfo", "supportedArchitectures"]).iter().filter_map(|i| i.text().map(str::to_string)).collect();
        let arch = if archs.iter().any(|a| a == "arm64") { "arm64" } else { "x86_64" };
        let vcpu = num(&["vCpuInfo", "defaultVCpus"]) as u32;
        let burstable = t(&["burstablePerformanceSupported"]).as_deref() == Some("true");
        out.push(Machine {
            family,
            kind: if burstable { "burstable" } else { kind },
            arch: arch.into(),
            vcpu,
            memory_gib: num(&["memoryInfo", "sizeInMiB"]) / 1024.0,
            burstable,
            local_gb: num(&["instanceStorageInfo", "totalSizeInGB"]) as u32,
            network: t(&["networkInfo", "networkPerformance"]).unwrap_or_default(),
            ebs_mbps: num(&["ebsInfo", "ebsOptimizedInfo", "baselineBandwidthInMbps"]) as u32,
            max_pods: max_pods(num(&["networkInfo", "maximumNetworkInterfaces"]) as u32, num(&["networkInfo", "ipv4AddressesPerInterface"]) as u32, vcpu),
            processor: None,
            usd_hour: None,
            spot_hour: None,
            name,
        });
    }
    Ok((out, text(root, &["nextToken"]).filter(|t| !t.is_empty())))
}

/// A page of `DescribeSpotPriceHistory` from now: the lowest price of each
/// type over the zones, and the token of the next page.
pub fn spot_prices(xml: &str, into: &mut HashMap<String, f64>) -> Result<Option<String>, String> {
    let d = parse(xml)?;
    let root = d.root_element();
    for it in items(root, &["spotPriceHistorySet"]) {
        let (Some(t), Some(p)) = (text(it, &["instanceType"]), text(it, &["spotPrice"]).and_then(|p| p.parse::<f64>().ok())) else { continue };
        let e = into.entry(t).or_insert(p);
        *e = e.min(p);
    }
    Ok(text(root, &["nextToken"]).filter(|t| !t.is_empty()))
}

/// The regions of `DescribeRegions`.
pub fn regions(xml: &str) -> Result<Vec<String>, String> {
    let d = parse(xml)?;
    let mut r: Vec<String> = items(d.root_element(), &["regionInfo"]).iter().filter_map(|i| text(*i, &["regionName"])).collect();
    r.sort();
    Ok(r)
}

/// The first on-demand price of a price list product, in US dollars per
/// its unit, with the product's attributes.
pub fn product_price(product: &Json) -> Option<(f64, &Json)> {
    let terms = product["terms"]["OnDemand"].as_object()?;
    let dims = terms.values().next()?["priceDimensions"].as_object()?;
    let usd = dims.values().next()?["pricePerUnit"]["USD"].as_str()?.parse::<f64>().ok()?;
    Some((usd, &product["product"]["attributes"]))
}

/// A page of `GetProducts` (AmazonEC2, compute instances): each type's
/// on-demand price an hour and processor; the token of the next page.
pub fn instance_prices(page: &Json, into: &mut HashMap<String, (f64, String)>) -> Result<Option<String>, String> {
    let list = page["PriceList"].as_array().ok_or("a price list page without PriceList")?;
    for p in list {
        // Each product is a JSON document in a string.
        let p: Json = match p {
            Json::String(s) => serde_json::from_str(s).map_err(|e| format!("a price list product: {e}"))?,
            other => other.clone(),
        };
        let Some((usd, a)) = product_price(&p) else { continue };
        let Some(t) = a["instanceType"].as_str() else { continue };
        // Some types list a zero price for a capacity they are not sold
        // as; a real price wins.
        if usd > 0.0 || !into.contains_key(t) {
            into.insert(t.to_string(), (usd, a["physicalProcessor"].as_str().unwrap_or_default().to_string()));
        }
    }
    Ok(page["NextToken"].as_str().filter(|t| !t.is_empty()).map(str::to_string))
}

/// The machines suggested for search: the newest generation of AWS
/// Graviton's general-purpose (`m`) and memory (`r`) families, large to
/// 2xlarge. Search keeps its segments and vectors in the page cache, so
/// memory matters as much as cores; Graviton costs less than x86 of the
/// same size, and Strata's image runs on both. Without Graviton in the
/// region, the newest x86 `m` and `r` families.
pub fn recommended(machines: &[Machine]) -> Vec<&Machine> {
    let generation = |f: &str, series: char| -> Option<u32> {
        let rest = f.strip_prefix(series)?;
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        let tail = &rest[digits.len()..];
        matches!(tail, "g" | "i" | "a").then_some(())?;
        digits.parse().ok()
    };
    let priced = |m: &&Machine| m.usd_hour.is_some_and(|p| p > 0.0);
    for arm in [true, false] {
        let mut out = Vec::new();
        for series in ['m', 'r'] {
            let of = |m: &&Machine| (m.arch == "arm64") == arm && generation(&m.family, series).is_some();
            let Some(newest) = machines.iter().filter(of).filter(priced).filter_map(|m| generation(&m.family, series)).max() else { continue };
            for size in ["large", "xlarge", "2xlarge"] {
                // m8g before m8i before m8a: the first priced family of the newest generation.
                if let Some(m) = machines.iter().filter(of).filter(priced).find(|m| generation(&m.family, series) == Some(newest) && m.name.ends_with(&format!(".{size}")) && m.name.split('.').nth(1) == Some(size)) {
                    out.push(m);
                }
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TYPES: &str = include_str!("../../tests/fixtures/aws/describe-instance-types.xml");
    const SPOT: &str = include_str!("../../tests/fixtures/aws/describe-spot-price-history.xml");
    const REGIONS: &str = include_str!("../../tests/fixtures/aws/describe-regions.xml");
    const PRICES: &str = include_str!("../../tests/fixtures/aws/get-products.json");

    #[test]
    fn ec2_answers_become_machines_with_prices() {
        // Real answers of us-east-1 (1 November 2026), request ids zeroed.
        let (m, next) = instance_types(TYPES).unwrap();
        assert!(next.is_none());
        let g = &m[0];
        assert_eq!((g.name.as_str(), g.family.as_str(), g.kind, g.arch.as_str(), g.vcpu, g.memory_gib, g.local_gb, g.ebs_mbps), ("m7g.large", "m7g", "general", "arm64", 2, 8.0, 0, 630));
        // 3 interfaces of 10 addresses: 3 * 9 + 2.
        assert_eq!((g.max_pods, g.network.as_str(), g.burstable), (29, "Up to 12.5 Gigabit", false));
        let i = &m[1];
        assert_eq!((i.name.as_str(), i.kind, i.arch.as_str(), i.vcpu, i.memory_gib, i.local_gb, i.max_pods), ("i4i.xlarge", "storage", "x86_64", 4, 32.0, 937, 58));

        let mut spot = HashMap::new();
        assert!(spot_prices(SPOT, &mut spot).unwrap().is_none());
        assert_eq!(spot.len(), 2);
        assert!(spot["i4i.xlarge"] <= 0.0892 && spot["m7g.large"] <= 0.0523, "{spot:?}");

        let mut prices = HashMap::new();
        assert!(instance_prices(&serde_json::from_str(PRICES).unwrap(), &mut prices).unwrap().is_none());
        assert_eq!(prices["m7g.large"], (0.0816, "AWS Graviton3 Processor".to_string()));
        assert_eq!(prices["i4i.xlarge"].0, 0.343);

        let r = regions(REGIONS).unwrap();
        assert!(r.contains(&"us-east-1".to_string()) && r.contains(&"eu-west-1".to_string()) && r.windows(2).all(|w| w[0] < w[1]), "{r:?}");

        let err = parse("<Response><Errors><Error><Code>AuthFailure</Code><Message>AWS was not able to validate the provided access credentials</Message></Error></Errors></Response>").unwrap_err();
        assert_eq!(err, "AuthFailure: AWS was not able to validate the provided access credentials");
    }

    #[test]
    fn families_are_sorted_into_what_they_are_for() {
        let k = |f| kind(f);
        assert_eq!([k("m8g"), k("c7i"), k("r8g"), k("x2gd"), k("i4i"), k("im4gn"), k("d3"), k("t4g"), k("m7i-flex")], [Some("general"), Some("compute"), Some("memory"), Some("memory"), Some("storage"), Some("storage"), Some("storage"), Some("burstable"), Some("general")]);
        assert_eq!([k("p5"), k("g6"), k("inf2"), k("trn1"), k("mac2"), k("hpc7g"), k("u-6tb1"), k("dl1"), k("f2"), k("vt1")], [None; 10]);
        assert_eq!(max_pods(8, 30, 48), 234);
        assert_eq!(max_pods(15, 50, 96), 250);
        assert_eq!(max_pods(4, 15, 8), 58);
        assert_eq!(max_pods(3, 6, 2), 17);

        let m = |name: &str, kind: &'static str, mem: f64, usd: f64| Machine { name: name.into(), family: name.split('.').next().unwrap().into(), kind, arch: if name.contains("g.") { "arm64" } else { "x86_64" }.into(), vcpu: 2, memory_gib: mem, burstable: kind == "burstable", local_gb: 0, network: String::new(), ebs_mbps: 0, max_pods: 29, processor: None, usd_hour: Some(usd), spot_hour: None };
        let all = [m("t4g.large", "burstable", 8.0, 0.0672), m("m7g.large", "general", 8.0, 0.0816), m("m8g.large", "general", 8.0, 0.0898), m("m8g.48xlarge", "general", 768.0, 8.6), m("r8g.large", "memory", 16.0, 0.1178), m("x8g.large", "memory", 32.0, 0.1), m("c8g.large", "compute", 4.0, 0.0798), m("r8g.xlarge", "memory", 32.0, 0.2356), m("m8i.large", "general", 8.0, 0.1058)];
        let r: Vec<&str> = recommended(&all).iter().map(|m| m.name.as_str()).collect();
        assert_eq!(r, ["m8g.large", "r8g.large", "r8g.xlarge"]);
        // No Graviton: the newest x86.
        let x86 = [m("m7i.large", "general", 8.0, 0.1008), m("m8i.large", "general", 8.0, 0.1058), m("r7i.large", "memory", 16.0, 0.1323)];
        let r: Vec<&str> = recommended(&x86).iter().map(|m| m.name.as_str()).collect();
        assert_eq!(r, ["m8i.large", "r7i.large"]);
    }
}
