use std::collections::HashSet;

use cni_plugin::{
	config::NetworkConfig, error::CniError, logger, reply::{reply, SuccessReply, Interface}, Cni, Command, Inputs, macaddr::MacAddr
};
use async_std::task::block_on;
use log::{debug, error, info};
use netlink_packet_route::link::{InfoBridge, InfoData, LinkAttribute, LinkInfo};
use rtnetlink::{packet_core::{NLM_F_ACK, NLM_F_REQUEST}, LinkBridge};

use crate::types::{BridgeNetConf, VlanTrunk, Bridge};
use crate::error::{AppError, VlanError};
use libcni::ns::ns::{self, Netns};
use libcni::ip::veth::{self, Veth};
use libcni::ip::link;

mod error;
mod types;
const BRIDGE_DEFAULT_NAME: &str = "cni0";

/// Entry point of the CNI bridge plugin.
fn main() {
    let mut logconfig = logger::default_config();
	logconfig.add_filter_ignore_str("netlink_proto");
	logger::with_config(env!("CARGO_PKG_NAME"), logconfig.build());

	debug!(
		"{} (CNI bridge plugin) version {}",
		env!("CARGO_PKG_NAME"),
		env!("CARGO_PKG_VERSION")
	);

	let inputs: Inputs = Cni::load().into_inputs().unwrap();
    let cni_version = inputs.config.cni_version.clone(); 

	info!(
		"{} serving spec v{} for command={:?}",
		env!("CARGO_PKG_NAME"),
		cni_version,
		inputs.command
	);

	let bridge_conf = match load_bri_netconf(inputs.config.clone()) {
        Ok(conf) => conf,
        Err(err) => {
            error!("Failed to load bridge config: {}", err);
            return;
        }
    };
	
	info!(
		"(CNI bridge plugin) version bridge config: {:?}",
		bridge_conf
	);

	let res:Result<SuccessReply,AppError> = block_on(async move {
		match inputs.command {
			Command::Add => cmd_add(bridge_conf, inputs).await,
			Command::Del => cmd_del(bridge_conf).await,
			Command::Check => todo!(),
			Command::Version => unreachable!(),
		}
	});
    
	match res {
        Ok(res) => {
            debug!("success! {:#?}", res);
            reply(res)
        }
        Err(res) => {
            error!("error: {}", res);
            reply(res.into_reply(cni_version))
        }
    }
}

/// Loads the bridge network configuration from a given `NetworkConfig`.
/// 
/// # Arguments
/// * `config` - The network configuration to be parsed.
/// 
/// # Returns
/// A `Result` containing the parsed `BridgeNetConf` or an `AppError` on failure.
pub fn load_bri_netconf(config: NetworkConfig) -> Result<BridgeNetConf, AppError> {
    let mut json_value = serde_json::to_value(&config).map_err(CniError::from)?;

	if !json_value.get("bridge").is_some() {
		json_value["bridge"] = serde_json::json!(BRIDGE_DEFAULT_NAME);
	}
	if !json_value.get("preserveDefaultVlan").is_some() {
		json_value["preserveDefaultVlan"] = serde_json::json!(true);
	}

    let mut bridge_conf: BridgeNetConf = serde_json::from_value(json_value).map_err(CniError::from)?;

    if let Some(vlan) = bridge_conf.vlan {
        if !(0..=4094).contains(&vlan) {
            return Err(CniError::InvalidField {
                field: "vlan", expected: "0 <= vlan <= 4094", value: vlan.into()
            }.into());
        }
    }

	bridge_conf.vlans = Some(collect_vlan_trunk(bridge_conf.vlan_trunk.as_deref())?);

 	if bridge_conf.vlan.is_some() && bridge_conf.vlans.is_some(){
		return Err((CniError::InvalidField { 
			field: "vlan", expected: "VLAN and VLAN Trunk cannot coexist", value: bridge_conf.vlan.into()
		}).into());
	}

	bridge_conf.mac = bridge_conf.args.as_ref().and_then(|args| args.mac.clone());
	if let Some(runtime) = &bridge_conf.net_conf.runtime {
        if let Some(mac_addr) = &runtime.mac {
            bridge_conf.mac = Some(mac_addr.to_string());
        }
    }
	
	Ok(bridge_conf)
}

/// Collects VLAN trunk IDs from a given optional list of `VlanTrunk`.
/// 
/// # Arguments
/// * `vlan_trunk` - Optional slice of VLAN trunk configurations.
/// 
/// # Returns
/// A `Result` containing a sorted list of unique VLAN IDs or an `AppError` on failure.
pub fn collect_vlan_trunk(vlan_trunk: Option<&[VlanTrunk]>) -> Result<Vec<i32>, AppError> {
    let Some(vlan_trunk) = vlan_trunk else {
        return Ok(vec![]);
    };

    let mut vlan_set: HashSet<i32> = HashSet::new();

    for item in vlan_trunk {
        match (item.min_id, item.max_id) {
            (Some(min_id), Some(max_id)) => {
                if !(1..=4094).contains(&min_id) {
                    return Err(VlanError::IncorrectMinID.into());
                }
                if !(1..=4094).contains(&max_id) {
                    return Err(VlanError::IncorrectMaxID.into());
                }
                if max_id < min_id {
                    return Err(VlanError::MinGreaterThanMax.into());
                }
                vlan_set.extend(min_id..=max_id);
            }
            (None, Some(_)) => return Err(VlanError::MissingMinID.into()),
            (Some(_), None) => return Err(VlanError::MissingMaxID.into()),
            _ => {}
        }

        if let Some(id) = item.id {
            if !(1..=4094).contains(&id) {
                return Err(AppError::from(VlanError::IncorrectTrunkID));
            }
            vlan_set.insert(id);
        }
    }

    let mut vlans: Vec<i32> = vlan_set.into_iter().collect();
    vlans.sort_unstable();
    Ok(vlans)
}

/// Retrieves an existing bridge by its name.
/// 
/// # Arguments
/// * `name` - The name of the bridge to retrieve.
/// 
/// # Returns
/// * `Ok(Bridge)` if the bridge exists and is valid.
/// * `Err(AppError)` if the bridge does not exist or is not of type bridge.
async fn bridge_by_name(name:&str) -> Result<Bridge, AppError> {
	let link = link::link_by_name(name).await
		.map_err(|e| AppError::NetlinkError(e.to_string()))?;

	let mut is_bridge = false;
	let mut vlan_filtering = false;
	let mut mtu: u32 = 1500;

	link.attributes.iter().for_each(|attr| {
		if let LinkAttribute::LinkInfo(link_info) = attr {
			link_info.iter().for_each(|info|{
				match info {
					LinkInfo::Kind(kind) if kind.to_string() == "bridge" => is_bridge = true, 
					LinkInfo::Data(InfoData::Bridge(bridge_attrs)) => {
						for bridge_attr in bridge_attrs {
							if let InfoBridge::VlanFiltering(v) = bridge_attr {
								vlan_filtering = *v;
							}
						}
					}	
					_ => {}
				} 
			});
		}
		if let LinkAttribute::Mtu(m) = attr {
            mtu = m.clone();
        }
	});

	if !is_bridge {
        return Err(AppError::NetlinkError(format!(
            "{} already exists but is not a bridge", name
        )));
    }

	Ok(Bridge::new(name)
	.mtu(mtu)
	.set_vlan_filtering(vlan_filtering))
}

/// Ensures a bridge exists with the given parameters. Creates it if necessary.
/// 
/// # Arguments
/// * `br_name` - The name of the bridge.
/// * `mtu` - Maximum Transmission Unit size.
/// * `promisc_mode` - Enable promiscuous mode.
/// * `vlan_filtering` - Enable VLAN filtering.
/// 
/// # Returns
/// * `Ok(Bridge)` if the bridge is created or exists with the required settings.
/// * `Err(AppError)` if bridge creation or configuration fails.
pub async fn ensure_bridge( 
	br_name: &str,
    mtu: u32,
    promisc_mode: bool,
    vlan_filtering: bool
) -> Result<Bridge, AppError> {
	let bridge = Bridge::new(br_name).mtu(mtu).set_vlan_filtering(vlan_filtering);
	let builder = bridge.clone().into_builder().up().promiscuous(promisc_mode);
	let link_message = builder.build();

	let add_result=link::add_link(link_message).await;
    if let Err(e) = add_result {
        if !e.to_string().contains("File exists") {
            return Err(AppError::NetlinkError(format!("Could not add {}: {}", br_name, e)));
        }
    }
	
	let br_link = link::link_by_name(br_name).await
        .map_err(|e| AppError::LinkError(format!("{}:{}", e, br_name)))?;

	let msg_builder = LinkBridge::new(br_name)
        .set_info_data(InfoData::Bridge(vec![InfoBridge::VlanFiltering(true)]));

    let mut msg = msg_builder.build();

    msg.header.index = br_link.header.index;

	let handle = link::get_handle()?.ok_or_else(|| AppError::LinkError(format!("Cannot get handle")))?;

	handle.link().add(msg).set_flags(NLM_F_ACK | NLM_F_REQUEST).execute()
	.await.map_err(|e|AppError::LinkError(format!("{}:{}", e, br_name)))?;

	bridge_by_name(br_name).await
}

/// Sets up a bridge with the provided network configuration.
/// 
/// # Arguments
/// * `n` - The bridge network configuration.
/// 
/// # Returns
/// * `Ok((Bridge, Interface))` containing the bridge and associated interface.
/// * `Err(AppError)` if the setup fails.
pub async fn setup_bridge(n: &BridgeNetConf) -> Result<(Bridge, Interface), AppError>{	
	let vlan_filtering = n.vlan.unwrap_or(0) != 0 || n.vlan_trunk.is_some();	
	let mtu = n.mtu.unwrap_or(1500);
	let name = n.br_name.as_deref().unwrap_or("cni0");
	
	let bridge = ensure_bridge( 
	name, mtu, n.promisc_mode.unwrap_or(false), vlan_filtering).await?;
	
	let link = link::link_by_name(name).await
		.map_err(|e| AppError::LinkError(format!("{}", e)))?;
	let mut ifname: String = Default::default();
	let mac_address = link::get_mac_address(&link.attributes);
	for attr in &link.attributes {
		if let LinkAttribute::IfName(name) = attr{
			ifname = name.clone();
		}
	}

	let interface = Interface {
        name: ifname,
        mac: mac_address,
		sandbox: Default::default(),
    };

	Ok((bridge,interface))
}

/// Sets up a virtual Ethernet (veth) pair between the host and the container network namespace.
/// 
/// # Arguments
/// * `host_ns` - The network namespace of the host.
/// * `netns` - The network namespace of the container.
/// * `br` - The bridge to which the veth pair will be attached.
/// * `if_name` - The name of the veth interface inside the container.
/// * `mtu` - The Maximum Transmission Unit (MTU) for the veth interface.
/// * `hairpin_mode` - Whether to enable hairpin mode on the veth peer.
/// * `_vlan_id`, `_vlans`, `_preserve_default_vlan`, `_port_isolation` - VLAN-related parameters (currently unused).
/// * `mac` - The MAC address of the container-side interface.
/// 
/// # Returns
/// * `Result<Veth, AppError>` - The created veth pair or an error if the operation fails.
pub async fn setup_veth(
    host_ns: &Netns,
    netns: &Netns,
    br: &Bridge,
    if_name: &str,
    mtu: u32,
    hairpin_mode: bool,
    _vlan_id: i32,
    _vlans: Vec<i32>,
    _preserve_default_vlan: bool,
    mac: &MacAddr,
	_port_isolation: bool,
) -> Result<Veth,AppError> {
	info!("netns: {:?}", netns.unique_id());
    info!("host_ns: {:?}", host_ns.unique_id());

	// Execute in the container's network namespace
	let mut veth = ns::exec_netns(host_ns,netns, async{
		let cur_ns = Netns::get()?;  
		anyhow::ensure!(&cur_ns == netns, "netns not match in main");
		let res = veth::setup_veth(if_name, "", mtu, mac, &host_ns, &netns).await?;
		Ok(res)
	}).await?;

	info!("veth: {:?}", veth);

	let br_link = link::link_by_name(&br.name).await
    	.map_err(|e| AppError::LinkError(format!("{}:{}", e, br.name)))?;

	let host_inf_link =link::link_by_name(&veth.peer_inf.name).await
		.map_err(|e| AppError::LinkError(format!("{}:{}", e, veth.peer_inf.name)))?;

	veth.peer_inf.mac = link::get_mac_address(&host_inf_link.attributes);

	link::link_set_master(&host_inf_link, &br_link).await
		.map_err(|e| AppError::LinkError(format!("can not set master{}", e)))?;

	link::link_set_hairpin(&host_inf_link, hairpin_mode).await
		.map_err(|e| AppError::LinkError(format!("can not set hairpin{}", e)))?;

	Ok(veth)
}

/// Adds a new container network interface to the bridge.
/// 
/// # Arguments
/// * `config` - The bridge network configuration.
/// * `inputs` - The CNI inputs containing network namespace and interface name.
/// 
/// # Returns
/// * `Result<SuccessReply, AppError>` - The success response with network details, or an error if failed.
async fn cmd_add(mut config: BridgeNetConf, inputs: Inputs) -> Result<SuccessReply,AppError>{
	let is_layer3 = config.net_conf.ipam.as_ref().map_or(false, |ipam| !ipam.r#plugin.is_empty());
	
	if is_layer3 && config.disable_container_interface.unwrap_or(false){
		return  Err(AppError::InvalidConfig(
			"Cannot use IPAM when DisableContainerInterface flag is set".to_string(),
		));
	}

	if config.is_default_gw .unwrap_or(false){
		config.is_gw = Some(true);
	}

	if config.hairpin_mode.unwrap_or(false) && config.promisc_mode.unwrap_or(false) {
		return Err(AppError::InvalidConfig(
			"Cannot set hairpin mode and promiscuous mode at the same time".to_string(),
		));
	}

	let (bridge, br_interface) = setup_bridge(&config).await?;

	let netns = if let Some(ref netns_path) = inputs.netns {
        Netns::get_from_path(netns_path).map_err(|e| {
            AppError::NetnsError(format!("failed to open netns {:?}: {}", netns_path, e))
        })?
    } else {
        return Err(AppError::NetnsError("netns path is None".to_string()));
    };	
	let current_ns = Netns::get()
		.map_err(|e| AppError::NetnsError(format!("failed to open current netns : {}", e)))?;

	let veth = setup_veth(
		&current_ns, 
		&netns.unwrap(),
		&bridge, 
		&inputs.ifname, 
		config.mtu.unwrap_or(1500), 
		config.hairpin_mode.unwrap_or(false),
		config.vlan.unwrap_or(0),
		config.vlans.unwrap_or(vec![]), 
		config.preserve_default_vlan.unwrap_or(false), 
		&Default::default(), 
		config.port_isolation.unwrap_or(false)
	).await.map_err(|e| AppError::VethError(format!("failed to set up veth : {}", e)))?;
	
	debug!(" veth :{:?}",veth);

	let (container_interface,host_interface)= veth.to_interface()
	.map_err(|e| AppError::VethError(format!("veth can not to interface : {}", e)))?;

	let bridge_result = SuccessReply {
		cni_version: config.net_conf.cni_version,
		interfaces: vec![container_interface,host_interface,br_interface],
		ips: Default::default(),
		routes: Default::default(),
		dns: Default::default(),
		specific: Default::default(),
	};
	Ok(bridge_result)
}

/// Deletes a container network interface from the bridge.
/// 
/// # Arguments
/// * `config` - The bridge network configuration.
/// 
/// # Returns
/// * `Result<SuccessReply, AppError>` - The success response or an error if deletion fails.
async fn cmd_del(config: BridgeNetConf) -> Result<SuccessReply,AppError>{
	Ok(SuccessReply {
		cni_version: config.net_conf.cni_version,
		interfaces: Default::default(),
		ips: Default::default(),
		routes: Default::default(),
		dns: Default::default(),
		specific: Default::default(),
	})
}

#[cfg(test)]
mod tests {

	use super::*;
	use std::collections::HashMap;
	use macaddr::MacAddr6;
use semver::Version;
	#[tokio::test]
	async fn test_setup_bridge() {
		let net_conf = NetworkConfig {
			cni_version:  Version::parse("0.4.0").expect("Invalid version format"),
			name: "test-network".to_string(),
			plugin: "bridge".to_string(),
			args: HashMap::new(),
			ip_masq: false,
			ipam: None,
			dns: None,
			runtime: None,
			prev_result: None,
			specific: HashMap::new(),
		};
	
		let bridge_net_conf = BridgeNetConf {
			net_conf,
			br_name: Some("test0".to_string()),
			is_gw: Some(true),
			is_default_gw: Some(false),
			force_address: Some(false),
			mtu: Some(1500),
			hairpin_mode: Some(true),
			promisc_mode: Some(false),
			vlan: Some(10),
			vlan_trunk: None,
			preserve_default_vlan: Some(true),
			mac_spoof_chk: Some(false),
			enable_dad: Some(true),
			disable_container_interface: Some(false),
			port_isolation: Some(false),
			args: None,
			mac: Some("00:11:22:33:44:55".to_string()),
			vlans: None,
		};
	
		let result = setup_bridge(&bridge_net_conf).await;
	
		match result {
			Ok((bridge, interface)) => {
				println!("Bridge created: {:?}", bridge);
				println!("Interface created: {:?}", interface);

				assert_eq!(bridge.name, "test0");	
				assert_eq!(bridge.mtu, 1500);	
				assert!(bridge.vlan_filtering);	
				assert!(!interface.name.is_empty());	
				assert!(interface.mac.is_some());

			}
			Err(e) => {
				panic!("setup_bridge failed: {:?}", e);
			}
		}
	}

	#[tokio::test]
	async fn test_setup_veth(){
		let host_ns = Netns::get().unwrap();
		let netns:Netns = Netns::get_from_name("test").unwrap().expect("can not get netns");
		let br = Bridge::new("test0");
		let if_name = "veth0";
    	let mtu = 1500;
    	let hairpin_mode = true;	
    	let vlan_id = 0;
    	let vlans = vec![];
    	let preserve_default_vlan = false;
    	let mac = &MacAddr::from(MacAddr6::new(0x00, 0x11, 0x22, 0x33, 0x44, 0x55));
		let port_isolation = false;

		let result = setup_veth(
			&host_ns, 
			&netns, 
			&br, 
			if_name, 
			mtu, 
			hairpin_mode,  
			vlan_id, 
			vlans, 
			preserve_default_vlan, 
			mac,
			port_isolation).await;
		
		match result {
			Ok(veth) => {
				println!("Veth:{:?}",veth);
				assert_eq!(veth.interface.name, "veth0");
			}
			Err(e) => {
				panic!("Expected Ok result, but got an error: {:?}", e);
				}
		}
	}
}
