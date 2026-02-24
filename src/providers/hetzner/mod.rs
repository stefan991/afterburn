// Copyright 2023 CoreOS, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Metadata fetcher for the hetzner provider
//! https://docs.hetzner.cloud/#server-metadata

use std::collections::HashMap;
use std::net::IpAddr;
use std::str::FromStr;

use anyhow::Result;
use ipnetwork::IpNetwork;
use openssh_keys::PublicKey;
use pnet_base::MacAddr;
use serde::Deserialize;
use slog_scope::warn;

use crate::network;
use crate::network::DhcpSetting;
use crate::retry;

use super::MetadataProvider;

#[cfg(test)]
mod mock_tests;

const HETZNER_METADATA_BASE_URL: &str = "http://169.254.169.254/hetzner/v1/metadata";

/// Metadata provider for Hetzner Cloud
///
/// See: https://docs.hetzner.cloud/#server-metadata
#[derive(Clone, Debug)]
pub struct HetznerProvider {
    client: retry::Client,
}

impl HetznerProvider {
    pub fn try_new() -> Result<Self> {
        let client = retry::Client::try_new()?;
        Ok(Self { client })
    }

    fn endpoint_for(key: &str) -> String {
        format!("{HETZNER_METADATA_BASE_URL}/{key}")
    }
}

impl MetadataProvider for HetznerProvider {
    fn attributes(&self) -> Result<HashMap<String, String>> {
        let metadata: Metadata = self
            .client
            .get(retry::Yaml, HETZNER_METADATA_BASE_URL.to_string())
            .send()?
            .unwrap();

        let private_networks: Vec<PrivateNetwork> = self
            .client
            .get(retry::Yaml, Self::endpoint_for("private-networks"))
            .send()?
            .unwrap();

        Ok(Attributes {
            metadata,
            private_networks,
        }
        .into())
    }

    fn hostname(&self) -> Result<Option<String>> {
        let hostname: String = self
            .client
            .get(retry::Raw, Self::endpoint_for("hostname"))
            .send()?
            .unwrap_or_default();

        if hostname.is_empty() {
            return Ok(None);
        }

        Ok(Some(hostname))
    }

    fn ssh_keys(&self) -> Result<Vec<PublicKey>> {
        let keys: Vec<String> = self
            .client
            .get(retry::Json, Self::endpoint_for("public-keys"))
            .send()?
            .unwrap_or_default();

        let keys = keys
            .iter()
            .map(|s| PublicKey::parse(s))
            .collect::<Result<_, _>>()?;

        Ok(keys)
    }

    // https://github.com/flatcar/Flatcar/issues/1968#issuecomment-3682009018
    fn networks(&self) -> Result<Vec<network::Interface>> {
        let network_config: NetworkConfig = self
            .client
            .get(retry::Yaml, Self::endpoint_for("network-config"))
            .send()?
            .unwrap();

        let interfaces = network_config
            .config
            .iter()
            .map(|entry| entry.to_interface())
            .collect::<Result<Vec<_>, _>>()?;

        Ok(interfaces)
    }

    // TODO: configure private networks as well
    // https://github.com/canonical/cloud-init/issues/4263
    // https://github.com/canonical/cloud-init/blob/07922ae05511aab160390a8ed18dfa6c1e03e57f/cloudinit/sources/DataSourceHetzner.py#L209


    // TODO: implement rd_network_kargs and netplan_config
    // https://coreos.github.io/afterburn/usage/initrd-network-cmdline/
    // see proxmoxve provider for an example of how to implement this

    fn rd_network_kargs(&self) -> Result<Option<String>> {
        let mut kargs = Vec::new();

        if let Ok(networks) = self.networks() {
            for iface in networks {
                // Add IP configuration if static
                for addr in iface.ip_addresses {
                    match addr {
                        IpNetwork::V4(network) => {
                            if let Some(gateway) = iface
                                .routes
                                .iter()
                                .find(|r| r.destination.is_ipv4() && r.destination.prefix() == 0)
                            {
                                kargs.push(format!(
                                    "ip={}::{}:{}",
                                    network.ip(),
                                    gateway.gateway,
                                    network.mask()
                                ));
                            } else {
                                kargs.push(format!("ip={}:::{}", network.ip(), network.mask()));
                            }
                        }
                        IpNetwork::V6(network) => {
                            if let Some(gateway) = iface
                                .routes
                                .iter()
                                .find(|r| r.destination.is_ipv6() && r.destination.prefix() == 0)
                            {
                                kargs.push(format!(
                                    "ip={}::{}:{}",
                                    network.ip(),
                                    gateway.gateway,
                                    network.prefix()
                                ));
                            } else {
                                kargs.push(format!("ip={}:::{}", network.ip(), network.prefix()));
                            }
                        }
                    }
                }

                // Add DHCP configuration
                if let Some(dhcp) = iface.dhcp {
                    match dhcp {
                        DhcpSetting::V4 => kargs.push("ip=dhcp".to_string()),
                        DhcpSetting::V6 => kargs.push("ip=dhcp6".to_string()),
                        DhcpSetting::Both => kargs.push("ip=dhcp,dhcp6".to_string()),
                    }
                }

                // Add nameservers
                if !iface.nameservers.is_empty() {
                    let nameservers = iface
                        .nameservers
                        .iter()
                        .map(|ns| ns.to_string())
                        .collect::<Vec<_>>()
                        .join(",");
                    kargs.push(format!("nameserver={}", nameservers));
                }
            }
        }

        if kargs.is_empty() {
            Ok(None)
        } else {
            Ok(Some(kargs.join(" ")))
        }
    }
}

#[derive(Debug, Deserialize)]
struct PrivateNetwork {
    ip: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Metadata {
    hostname: Option<String>,
    instance_id: Option<i64>,
    public_ipv4: Option<String>,
    availability_zone: Option<String>,
    region: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NetworkConfig {
    config: Vec<Interface>,
    // version: i64,
}

#[derive(Debug, Deserialize)]
struct Interface {
    mac_address: Option<String>,
    name: Option<String>,
    subnets: Vec<Subnet>,
    // r#type: String,
}

#[derive(Debug, Deserialize)]
struct Subnet {
    address: Option<String>,
    dns_nameservers: Option<Vec<String>>,
    gateway: Option<String>,
    ipv4: Option<bool>,
    ipv6: Option<bool>,
    r#type: String,
}

struct Attributes {
    metadata: Metadata,
    private_networks: Vec<PrivateNetwork>,
}

impl From<Attributes> for HashMap<String, String> {
    fn from(attributes: Attributes) -> Self {
        let mut out = HashMap::with_capacity(5);

        let add_value = |map: &mut HashMap<_, _>, key: &str, value: Option<String>| {
            if let Some(value) = value {
                map.insert(key.to_string(), value);
            }
        };

        add_value(
            &mut out,
            "HETZNER_AVAILABILITY_ZONE",
            attributes.metadata.availability_zone,
        );
        add_value(&mut out, "HETZNER_HOSTNAME", attributes.metadata.hostname);
        add_value(
            &mut out,
            "HETZNER_INSTANCE_ID",
            attributes.metadata.instance_id.map(|i| i.to_string()),
        );
        add_value(
            &mut out,
            "HETZNER_PUBLIC_IPV4",
            attributes.metadata.public_ipv4,
        );
        add_value(&mut out, "HETZNER_REGION", attributes.metadata.region);

        for (i, a) in attributes.private_networks.iter().enumerate() {
            add_value(
                &mut out,
                format!("HETZNER_PRIVATE_IPV4_{i}").as_str(),
                a.ip.clone(),
            );
        }

        out
    }
}

impl Interface {
    fn to_interface(&self) -> Result<network::Interface> {
        let mut result = network::Interface {
            name: self.name.clone(),

            // filled below because Option::try_map doesn't exist yet
            mac_address: None,

            // filled below
            ip_addresses: vec![],
            // filled below
            routes: vec![],
            // filled below
            nameservers: vec![],
            // filled below
            dhcp: None,

            // default values
            path: None,
            priority: 20,
            bond: None,
            unmanaged: false,
            required_for_online: None,
        };

        if let Some(mac) = &self.mac_address {
            result.mac_address = Some(MacAddr::from_str(mac)?);
        }

        let mut ipv4_dhcp = false;
        let mut ipv6_dhcp = false;

        for subnet in &self.subnets {
            if subnet.ipv4 == Some(true) && subnet.r#type.contains("dhcp") {
                ipv4_dhcp = true;
            }

            if subnet.ipv6 == Some(true) && subnet.r#type.contains("dhcp") {
                ipv6_dhcp = true;
            }

            if subnet.r#type.contains("static")
            {
                if subnet.address.is_none() {
                    return Err(anyhow::anyhow!(
                        "cannot convert static subnet to interface: missing address"
                    ));
                }

                // TODO: check if it matches the ivp4/ipv6 flags and error if not?
                result
                    .ip_addresses
                    .push(IpNetwork::from_str(subnet.address.as_ref().unwrap())?);

                if let Some(gateway) = &subnet.gateway {
                    let gateway = IpAddr::from_str(gateway)?;

                    let destination = if gateway.is_ipv6() {
                        IpNetwork::from_str("::/0")?
                    } else {
                        IpNetwork::from_str("0.0.0.0/0")?
                    };

                    result.routes.push(network::NetworkRoute {
                        destination,
                        gateway,
                    });
                } else {
                    warn!("found subnet type \"static\" without gateway");
                }

                if let Some(dns_nameservers) = &subnet.dns_nameservers {
                    for dns in dns_nameservers {
                        result.nameservers.push(IpAddr::from_str(dns)?);
                    }
                } else {
                    warn!("found subnet type \"static\" without nameservers");
                }
            }
        }

        result.dhcp = match (ipv4_dhcp, ipv6_dhcp) {
            (false, false) => None,
            (true, false) => Some(network::DhcpSetting::V4),
            (false, true) => Some(network::DhcpSetting::V6),
            (true, true) => Some(network::DhcpSetting::Both),
        };

        return Ok(result);
    }
}

#[cfg(test)]
mod tests {
    use super::{Metadata, NetworkConfig, PrivateNetwork};

    #[test]
    fn test_metadata_deserialize() {
        let body = r#"availability-zone: hel1-dc2
hostname: my-server
instance-id: 42
public-ipv4: 1.2.3.4
region: eu-central
public-keys: []"#;

        let meta: Metadata = serde_yaml::from_str(body).unwrap();

        assert_eq!(meta.availability_zone.unwrap(), "hel1-dc2");
        assert_eq!(meta.hostname.unwrap(), "my-server");
        assert_eq!(meta.instance_id.unwrap(), 42);
        assert_eq!(meta.public_ipv4.unwrap(), "1.2.3.4");
    }

    #[test]
    fn test_private_networks_deserialize() {
        let body = r"- ip: 10.0.0.2
  alias_ips: []
  interface_num: 2
  mac_address: 86:00:00:98:40:6e
  network_id: 4124728
  network_name: foo
  network: 10.0.0.0/16
  subnet: 10.0.0.0/24
  gateway: 10.0.0.1
- ip: 10.128.0.2
  alias_ips: []
  interface_num: 1
  mac_address: 86:00:00:98:40:6d
  network_id: 4451335
  network_name: bar
  network: 10.128.0.0/16
  subnet: 10.128.0.0/16
  gateway: 10.128.0.1";

        let private_networks: Vec<PrivateNetwork> = serde_yaml::from_str(body).unwrap();

        assert_eq!(private_networks.len(), 2);
        assert_eq!(private_networks[0].ip.clone().unwrap(), "10.0.0.2");
        assert_eq!(private_networks[1].ip.clone().unwrap(), "10.128.0.2");
    }

    #[test]
    fn test_network_config_deserialize() {
        let body = r"
config:
- mac_address: 92:00:07:2f:3f:0a
  name: eth0
  subnets:
  - ipv4: true
    type: dhcp
  - address: 2a01:4f8:1c1a:744f::1/64
    dns_nameservers:
    - 2a01:4ff:ff00::add:2
    - 2a01:4ff:ff00::add:1
    gateway: fe80::1
    ipv6: true
    type: static
  type: physical
version: 1";

        let network_config: NetworkConfig = serde_yaml::from_str(body).unwrap();

        assert_eq!(network_config.config.len(), 1);
        assert_eq!(network_config.config[0].mac_address, Some("92:00:07:2f:3f:0a".to_string()));
        assert_eq!(network_config.config[0].name, Some("eth0".to_string()));
        assert_eq!(network_config.config[0].subnets.len(), 2);
        assert_eq!(network_config.config[0].subnets[0].ipv4, Some(true));
        assert_eq!(network_config.config[0].subnets[0].r#type, "dhcp");
        assert_eq!(network_config.config[0].subnets[1].address, Some("2a01:4f8:1c1a:744f::1/64".to_string()));
        assert_eq!(network_config.config[0].subnets[1].dns_nameservers.as_ref().unwrap().len(), 2);
        assert_eq!(network_config.config[0].subnets[1].dns_nameservers.as_ref().unwrap()[0], "2a01:4ff:ff00::add:2");
        assert_eq!(network_config.config[0].subnets[1].dns_nameservers.as_ref().unwrap()[1], "2a01:4ff:ff00::add:1");
        assert_eq!(network_config.config[0].subnets[1].gateway, Some("fe80::1".to_string()));
        assert_eq!(network_config.config[0].subnets[1].ipv6, Some(true));
        assert_eq!(network_config.config[0].subnets[1].r#type, "static");
    }
}
