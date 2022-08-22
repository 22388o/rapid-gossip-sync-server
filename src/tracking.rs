use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bitcoin::hashes::hex::ToHex;
use bitcoin::secp256k1::{PublicKey, SecretKey};
use futures::executor;
use lightning;
use lightning::ln::peer_handler::{
	ErroringMessageHandler, IgnoringMessageHandler, MessageHandler, PeerManager,
};
use lightning::routing::gossip::{NetworkGraph, P2PGossipSync};
use rand::{Rng, thread_rng};
use tokio::sync::mpsc;

use crate::{config, TestLogger};
use crate::downloader::{GossipCounter, GossipRouter};
use crate::types::{DetectedGossipMessage, GossipChainAccess, GossipMessage, GossipPeerManager};
use crate::verifier::ChainVerifier;

pub(crate) async fn download_gossip(persistence_sender: mpsc::Sender<DetectedGossipMessage>, network_graph: Arc<NetworkGraph<Arc<TestLogger>>>) {
	let mut key = [0; 32];
	let mut random_data = [0; 32];
	thread_rng().fill_bytes(&mut key);
	thread_rng().fill_bytes(&mut random_data);
	let our_node_secret = SecretKey::from_slice(&key).unwrap();

	let _arc_chain_access = None::<GossipChainAccess>;
	let arc_chain_access = Some(Arc::new(ChainVerifier::new()));
	let ignorer = IgnoringMessageHandler {};
	let arc_ignorer = Arc::new(ignorer);

	let errorer = ErroringMessageHandler::new();
	let arc_errorer = Arc::new(errorer);

	let logger = TestLogger::new();
	let arc_logger = Arc::new(logger);

	let router = P2PGossipSync::new(
		network_graph.clone(),
		arc_chain_access,
		Arc::clone(&arc_logger),
	);
	let arc_router = Arc::new(router);
	let wrapped_router = GossipRouter {
		native_router: arc_router,
		counter: RwLock::new(GossipCounter::new()),
		sender: persistence_sender.clone(),
	};
	let arc_wrapped_router = Arc::new(wrapped_router);

	let message_handler = MessageHandler {
		chan_handler: arc_errorer,
		route_handler: arc_wrapped_router.clone(),
	};
	let peer_handler = PeerManager::new(
		message_handler,
		our_node_secret,
		&random_data,
		Arc::clone(&arc_logger),
		arc_ignorer,
	);
	let arc_peer_handler = Arc::new(peer_handler);

	println!("Connecting to Lightning peers…");
	let peers = config::ln_peers();
	let mut connected_peer_count = 0;

	for current_peer in peers {
		let initial_connection_succeeded = monitor_peer_connection(current_peer, Arc::clone(&arc_peer_handler));
		if initial_connection_succeeded {
			connected_peer_count += 1;
		}
	}

	if connected_peer_count < 1 {
		panic!("Failed to connect to any peer.");
	}

	println!("Connected to {} Lightning peers!", connected_peer_count);

	let local_router = arc_wrapped_router.clone();
	let local_persistence_sender = persistence_sender.clone();
	tokio::spawn(async move {
		let mut previous_announcement_count = 0u64;
		let mut previous_update_count = 0u64;
		let mut is_caught_up_with_gossip = false;

		let mut i = 0u32;
		let mut latest_new_gossip_time = Instant::now();
		let mut needs_to_notify_persister = false;

		loop {
			i += 1; // count the background activity
			let sleep = tokio::time::sleep(Duration::from_secs(5));
			sleep.await;

			let current_timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
			let router_clone = Arc::clone(&local_router);

			{
				let counter = router_clone.counter.read().unwrap();
				let total_message_count = counter.channel_announcements + counter.channel_updates;
				let new_message_count = total_message_count - previous_announcement_count - previous_update_count;

				let was_previously_caught_up_with_gossip = is_caught_up_with_gossip;
				// TODO: make new message threshold (20) adjust based on connected peer count
				is_caught_up_with_gossip = new_message_count < 20 && previous_announcement_count > 0 && previous_update_count > 0;
				if new_message_count > 0 {
					latest_new_gossip_time = Instant::now();
				}

				// if we either aren't caught up, or just stopped/started being caught up
				if !is_caught_up_with_gossip || (is_caught_up_with_gossip != was_previously_caught_up_with_gossip) {
					println!(
						"gossip count (iteration {}): {} (delta: {}):\n\tannouncements: {}\n\t\tmismatched scripts: {}\n\tupdates: {}\n\t\tno HTLC max: {}\n",
						i,
						total_message_count,
						new_message_count,
						counter.channel_announcements,
						counter.channel_announcements_with_mismatched_scripts,
						counter.channel_updates,
						counter.channel_updates_without_htlc_max_msats
					);
				} else {
					println!("Monitoring for gossip…")
				}

				if is_caught_up_with_gossip && !was_previously_caught_up_with_gossip {
					println!("caught up with gossip!");
					needs_to_notify_persister = true;
				} else if !is_caught_up_with_gossip && was_previously_caught_up_with_gossip {
					println!("Received new messages since catching up with gossip!");
				}

				let continuous_caught_up_duration = latest_new_gossip_time.elapsed();
				if continuous_caught_up_duration.as_secs() > 600 {
					eprintln!("No new gossip messages in 10 minutes! Something's amiss!");
				}

				previous_announcement_count = counter.channel_announcements;
				previous_update_count = counter.channel_updates;
			}

			if needs_to_notify_persister {
				needs_to_notify_persister = false;
				let sender = local_persistence_sender.clone();
				tokio::spawn(async move {
					let _ = sender.send(DetectedGossipMessage {
						timestamp_seen: current_timestamp as u32,
						message: GossipMessage::InitialSyncComplete,
					}).await;
				});
			}
		}
	});
}

fn monitor_peer_connection(current_peer: (PublicKey, SocketAddr), peer_manager: GossipPeerManager) -> bool {
	let peer_manager_clone = Arc::clone(&peer_manager);
	eprintln!("Connecting to peer {}@{}…", current_peer.0.to_hex(), current_peer.1.to_string());
	let connection = executor::block_on(async move {
		lightning_net_tokio::connect_outbound(
			peer_manager_clone,
			current_peer.0,
			current_peer.1,
		).await
	});
	let mut initial_connection_succeeded = false;
	if let Some(disconnection_future) = connection {
		eprintln!("Connected to peer {}@{}!", current_peer.0.to_hex(), current_peer.1.to_string());
		initial_connection_succeeded = true;
		let peer_manager_clone = Arc::clone(&peer_manager);
		tokio::spawn(async move {
			disconnection_future.await;
			eprintln!("Disconnected from peer {}@{}", current_peer.0.to_hex(), current_peer.1.to_string());
			monitor_peer_connection(current_peer.clone(), peer_manager_clone);
		});
	} else {
		eprintln!("Failed to connect to peer {}@{}", current_peer.0.to_hex(), current_peer.1.to_string())
	};
	initial_connection_succeeded
}