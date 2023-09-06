use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::net::SocketAddr;
use std::ops::Deref;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bitcoin::hashes::hex::ToHex;
use bitcoin::secp256k1::PublicKey;
use lightning;
use lightning::ln::peer_handler::{
	ErroringMessageHandler, IgnoringMessageHandler, MessageHandler, PeerManager,
};
use lightning::{log_info, log_warn};
use lightning::routing::gossip::NetworkGraph;
use lightning::sign::KeysManager;
use lightning::util::logger::Logger;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::config;
use crate::downloader::GossipRouter;
use crate::types::{GossipMessage, GossipPeerManager};

pub(crate) async fn download_gossip<L: Deref + Clone + Send + Sync + 'static>(persistence_sender: mpsc::Sender<GossipMessage>,
	completion_sender: mpsc::Sender<()>,
	network_graph: Arc<NetworkGraph<L>>,
	logger: L,
) where L::Target: Logger {
	let mut key = [42; 32];
	let mut random_data = [43; 32];
	// Get something psuedo-random from std.
	let mut key_hasher = RandomState::new().build_hasher();
	key_hasher.write_u8(1);
	key[0..8].copy_from_slice(&key_hasher.finish().to_ne_bytes());
	let mut rand_hasher = RandomState::new().build_hasher();
	rand_hasher.write_u8(2);
	random_data[0..8].copy_from_slice(&rand_hasher.finish().to_ne_bytes());

	let keys_manager = Arc::new(KeysManager::new(&key, 0xdeadbeef, 0xdeadbeef));

	let router = Arc::new(GossipRouter::new(network_graph, persistence_sender.clone(), logger.clone()));

	let message_handler = MessageHandler {
		chan_handler: ErroringMessageHandler::new(),
		route_handler: Arc::clone(&router),
		onion_message_handler: IgnoringMessageHandler {},
		custom_message_handler: IgnoringMessageHandler {},
	};
	let peer_handler = Arc::new(PeerManager::new(
		message_handler,
		0xdeadbeef,
		&random_data,
		logger.clone(),
		keys_manager,
	));
	router.set_pm(Arc::clone(&peer_handler));

	let ph_timer = Arc::clone(&peer_handler);
	tokio::spawn(async move {
		let mut intvl = tokio::time::interval(Duration::from_secs(10));
		loop {
			intvl.tick().await;
			ph_timer.timer_tick_occurred();
		}
	});

	log_info!(logger, "Connecting to Lightning peers...");
	let peers = config::ln_peers();
	let mut handles = JoinSet::new();
	let mut connected_peer_count = 0;

	if peers.len() <= config::CONNECTED_PEER_ASSERTION_LIMIT {
		log_warn!(logger, "Peer assertion threshold is {}, but only {} peers specified.", config::CONNECTED_PEER_ASSERTION_LIMIT, peers.len());
	}

	for current_peer in peers {
		let peer_handler_clone = peer_handler.clone();
		let logger_clone = logger.clone();
		handles.spawn(async move {
			connect_peer(current_peer, peer_handler_clone, logger_clone).await
		});
	}

	while let Some(connection_result) = handles.join_next().await {
		if let Ok(connection) = connection_result {
			if connection {
				connected_peer_count += 1;
				if connected_peer_count >= config::CONNECTED_PEER_ASSERTION_LIMIT {
					break;
				}
			}
		}
	}

	if connected_peer_count < 1 {
		panic!("Failed to connect to any peer.");
	}

	log_info!(logger, "Connected to {} Lightning peers!", connected_peer_count);

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

		{
			let counter = router.counter.read().unwrap();
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
				log_info!(
					logger,
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
				log_info!(logger, "Monitoring for gossip…")
			}

			if is_caught_up_with_gossip && !was_previously_caught_up_with_gossip {
				log_info!(logger, "caught up with gossip!");
				needs_to_notify_persister = true;
			} else if !is_caught_up_with_gossip && was_previously_caught_up_with_gossip {
				log_info!(logger, "Received new messages since catching up with gossip!");
			}

			let continuous_caught_up_duration = latest_new_gossip_time.elapsed();
			if continuous_caught_up_duration.as_secs() > 600 {
				log_warn!(logger, "No new gossip messages in 10 minutes! Something's amiss!");
			}

			previous_announcement_count = counter.channel_announcements;
			previous_update_count = counter.channel_updates;
		}

		if needs_to_notify_persister {
			needs_to_notify_persister = false;
			completion_sender.send(()).await.unwrap();
		}
	}
}

async fn connect_peer<L: Deref + Clone + Send + Sync + 'static>(current_peer: (PublicKey, SocketAddr), peer_manager: GossipPeerManager<L>, logger: L) -> bool where L::Target: Logger {
	// we seek to find out if the first connection attempt was successful
	let (sender, mut receiver) = mpsc::channel::<bool>(1);
	tokio::spawn(async move {
		log_info!(logger, "Connecting to peer {}@{}...", current_peer.0.to_hex(), current_peer.1.to_string());
		let mut is_first_iteration = true;
		loop {
			if let Some(disconnection_future) = lightning_net_tokio::connect_outbound(
				Arc::clone(&peer_manager),
				current_peer.0,
				current_peer.1,
			).await {
				log_info!(logger, "Connected to peer {}@{}!", current_peer.0.to_hex(), current_peer.1.to_string());
				if is_first_iteration {
					sender.send(true).await.unwrap();
				}
				disconnection_future.await;
				log_warn!(logger, "Disconnected from peer {}@{}...", current_peer.0.to_hex(), current_peer.1.to_string());
				tokio::time::sleep(Duration::from_secs(10)).await;
				log_warn!(logger, "Reconnecting to peer {}@{}...", current_peer.0.to_hex(), current_peer.1.to_string());
			} else {
				if is_first_iteration {
					sender.send(false).await.unwrap();
				}
			}
			is_first_iteration = false;
		}
	});

	let success = receiver.recv().await.unwrap();
	success
}
