/* The Wizards' Wallet
 * Written in 2014 by
 *   Andrew Poelstra <apoelstra@wpsoftware.net>
 *
 * To the extent possible under law, the author(s) have dedicated all
 * copyright and related and neighboring rights to this software to
 * the public domain worldwide. This software is distributed without
 * any warranty.
 *
 * You should have received a copy of the CC0 Public Domain Dedication
 * along with this software.
 * If not, see <http://creativecommons.org/publicdomain/zero/1.0/>.
 */

use std::io::IoResult;
use std::path::posix::Path;

use bitcoin::blockdata::blockchain::Blockchain;
use bitcoin::blockdata::utxoset::UtxoSet;
use bitcoin::network::constants::Network;
use bitcoin::network::serialize::Serializable;
use bitcoin::network::listener::Listener;
use bitcoin::network::socket::Socket;
use bitcoin::network::message::NetworkMessage;
use bitcoin::network::message;
use bitcoin::network::message_blockdata::{GetHeadersMessage, Inventory, InvBlock};
use bitcoin::util::patricia_tree::PatriciaTree;
use bitcoin::util::misc::consume_err;
use bitcoin::util::hash::zero_hash;

use constants::BLOCKCHAIN_N_FULL_BLOCKS;
use constants::UTXO_SYNC_N_BLOCKS;

/// We use this IdleState structure to avoid having Option<T>
/// on some stuff that isn't available during bootstrap.
struct IdleState {
  sock: Socket,
  net_chan: Receiver<NetworkMessage>,
  blockchain: Blockchain,
  utxo_set: UtxoSet
}

enum StartupState {
  Init,
  LoadFromDisk(Socket, Receiver<NetworkMessage>),
  SyncBlockchain(IdleState),
  SyncUtxoSet(IdleState, Vec<Inventory>),
  SaveToDisk(IdleState), 
  Idle(IdleState)
}

pub struct Bitcoind {
  network: Network,
  peer_address: String,
  peer_port: u16,
  blockchain_path: Path,
  utxo_set_path: Path
}

macro_rules! with_next_message(
  ( $recv:expr, $( $name:pat => $code:expr )* ) => (
    {
      let mut ret;
      loop {
        match $recv {
          $(
            $name => {
              ret = $code;
              break;
            },
          )*
          _ => {}
        };
      }
      ret
    }
  )
)

impl Bitcoind {
  pub fn new(peer_address: &str, peer_port: u16, network: Network,
             blockchain_path: Path, utxo_set_path: Path) -> Bitcoind {
    Bitcoind {
      peer_address: String::from_str(peer_address),
      peer_port: peer_port,
      network: network,
      blockchain_path: blockchain_path,
      utxo_set_path: utxo_set_path
    }
  }

  /// Run the state machine
  pub fn listen(&mut self) -> IoResult<()> {
    let mut state = Init;
    // Eternal state machine loop
    loop {
      state = match state {
        // First startup
        Init => {
          // Open socket
          let (chan, sock) = try!(self.start());
          LoadFromDisk(sock, chan)
        }
        // Load cached blockchain and utxo set from disk
        LoadFromDisk(sock, chan) => {
          println!("Loading blockchain...");
          // Load blockchain from disk
          let blockchain = match Serializable::deserialize_file(&self.blockchain_path) {
            Ok(blockchain) => blockchain,
            Err(e) => {
              println!("Failed to load blockchain: {:}, starting from genesis.", e);
              Blockchain::new(self.network)
            }
          };
          println!("Loading utxo set...");
          let utxo_set = match Serializable::deserialize_file(&self.utxo_set_path) {
            Ok(utxo_set) => utxo_set,
            Err(e) => {
              println!("Failed to load UTXO set: {:}, starting from genesis.", e);
              UtxoSet::new(self.network, BLOCKCHAIN_N_FULL_BLOCKS)
            }
          };

          SyncBlockchain(IdleState {
              sock: sock,
              net_chan: chan,
              blockchain: blockchain,
              utxo_set: utxo_set
            })
        },
        // Synchronize the blockchain with the peer
        SyncBlockchain(mut idle_state) => {
          // Do a headers-first sync of all blocks
          println!("Headers sync: last best tip {}", idle_state.blockchain.best_tip_hash());
          let mut done = false;
          while !done {
            // Request headers
            consume_err("Headers sync: failed to send `headers` message",
              idle_state.sock.send_message(message::GetHeaders(
                  GetHeadersMessage::new(idle_state.blockchain.locator_hashes(),
                                         zero_hash()))));
            // Loop through received headers
            let mut received_headers = false;
            while !received_headers {
              with_next_message!(idle_state.net_chan.recv(),
                message::Headers(headers) => {
                  for lone_header in headers.iter() {
                    match idle_state.blockchain.add_header(lone_header.header) {
                      Err(e) => {
                        println!("Headers sync: failed to add {}: {}", lone_header.header.hash(), e);
                      }
                       _ => {}
                    }
                  }
                  received_headers = true;
                  // We are done if this `headers` message did not update our status
                  done = headers.len() == 0;
                }
                message::Ping(nonce) => {
                  consume_err("Warning: failed to send pong in response to ping",
                    idle_state.sock.send_message(message::Pong(nonce)));
                }
              );
            }
          }
          // Done!
          println!("Done sync.");
          SyncUtxoSet(idle_state, Vec::with_capacity(UTXO_SYNC_N_BLOCKS))
        },
        SyncUtxoSet(mut idle_state, mut cache) => {
          let last_hash = idle_state.utxo_set.last_hash();
          println!("utxo set last hash {}", last_hash);
          let mut failed = false;

          cache.clear();
          // Unwind any reorg'd blooks
          for block in idle_state.blockchain.rev_stale_iter(last_hash) {
            println!("Rewinding stale block {}", block.header.hash());
            if !idle_state.utxo_set.rewind(block) {
              println!("Failed to rewind stale block {}", block.header.hash());
            }
          }
          // Loop through blockchain for new data
          let last_hash = idle_state.utxo_set.last_hash();
          for (count, node) in idle_state.blockchain.iter(last_hash).enumerate().skip(1) {
            cache.push(Inventory { inv_type: InvBlock, hash: node.block.header.hash() });

            // Every so often, send a new message
            if count % UTXO_SYNC_N_BLOCKS == 0 {
              println!("Sending getdata, count {} n_utxos {}", count, idle_state.utxo_set.n_utxos());
              consume_err("UTXO sync: failed to send `getdata` message",
                idle_state.sock.send_message(message::GetData(cache.clone())));

              let mut block_count = 0;
              let mut recv_data = PatriciaTree::new();
              while block_count < UTXO_SYNC_N_BLOCKS {
                with_next_message!(idle_state.net_chan.recv(),
                  message::Block(block) => {
                    recv_data.insert(&block.header.hash().as_uint128(), 128, block);
                    block_count += 1;
                  }
                  message::NotFound(_) => {
                    println!("UTXO sync: received `notfound` from sync peer, failing sync.");
                    failed = true;
                    block_count += 1;
                  }
                  message::Ping(nonce) => {
                    consume_err("Warning: failed to send pong in response to ping",
                      idle_state.sock.send_message(message::Pong(nonce)));
                  }
                )
              }
              for recv_inv in cache.iter() {
                let block_opt = recv_data.lookup(&recv_inv.hash.as_uint128(), 128);
                match block_opt {
                  Some(block) => {
                    if !idle_state.utxo_set.update(block) {
                      println!("Failed to update UTXO set with block {}", block.header.hash());
                      failed = true;
                    }
                  }
                  None => {
                    println!("Uh oh, requested block {} but didn't get it!", recv_inv.hash);
                    failed = true;
                  }
                }
              }
              cache.clear();
            }
          }
          if failed {
            println!("Failed to sync UTXO set, trying to resync chain.");
            SyncBlockchain(idle_state)
          } else {
            // Now that we're done with reorgs, update our cached block data
            let mut hashes_to_drop_data = vec![];
            let mut inv_to_add_data = vec![];
            for (n, node) in idle_state.blockchain.rev_iter(idle_state.blockchain.best_tip_hash()).enumerate() {
              if n < BLOCKCHAIN_N_FULL_BLOCKS {
                if !node.has_txdata {
                  inv_to_add_data.push(Inventory { inv_type: InvBlock,
                                                   hash: node.block.header.hash() });
                }
              } else if node.has_txdata {
                hashes_to_drop_data.push(node.block.header.hash());
              }
            }
            // Request new block data
            consume_err("UTXO sync: failed to send `getdata` message",
              idle_state.sock.send_message(message::GetData(inv_to_add_data.clone())));
            // Delete old block data
            for hash in hashes_to_drop_data.move_iter() {
              println!("Dropping old blockdata for {}", hash);
              match idle_state.blockchain.remove_txdata(hash) {
                Err(e) => { println!("Failed to remove txdata: {}", e); }
                _ => {}
              }
            }
            // Receive new block data
            let mut block_count = 0;
            while block_count < inv_to_add_data.len() {
              with_next_message!(idle_state.net_chan.recv(),
                message::Block(block) => {
                  println!("Adding blockdata for {}", block.header.hash());
                  match idle_state.blockchain.add_txdata(block) {
                    Err(e) => { println!("Failed to add txdata: {}", e); }
                    _ => {}
                  }
                  block_count += 1;
                }
                message::NotFound(_) => {
                  println!("Blockchain sync: received `notfound` on full blockdata, will not be able to handle reorgs past this block.");
                  block_count += 1;
                }
                message::Ping(nonce) => {
                  consume_err("Warning: failed to send pong in response to ping",
                    idle_state.sock.send_message(message::Pong(nonce)));
                }
              )
            }
            SaveToDisk(idle_state)
          }
        },
        // Idle loop
        Idle(mut idle_state) => {
          println!("Idling...");
          let recv = idle_state.net_chan.recv();
          idle_message(&mut idle_state, recv);
          Idle(idle_state)
        },
        // Temporary states
        SaveToDisk(idle_state) => {
          println!("Saving blockchain...");
          match idle_state.blockchain.serialize_file(&self.blockchain_path) {
            Ok(()) => { println!("Successfully saved blockchain.") },
            Err(e) => { println!("failed to write blockchain: {:}", e); }
          }
          println!("Saving UTXO set...");
          match idle_state.utxo_set.serialize_file(&self.utxo_set_path) {
            Ok(()) => { println!("Successfully saved UTXO set.") },
            Err(e) => { println!("failed to write UTXO set: {:}", e); }
          }
          Idle(idle_state)
        }
      };
    }
  }
}

impl Listener for Bitcoind {
  fn peer<'a>(&'a self) -> &'a str {
    self.peer_address.as_slice()
  }

  fn port(&self) -> u16 {
    self.peer_port
  }

  fn network(&self) -> Network {
    self.network
  }
}

/// Idle message handler
fn idle_message(idle_state: &mut IdleState, message: NetworkMessage) {
  match message {
    message::Version(_) => {
      // TODO: actually read version message
      consume_err("Warning: failed to send getdata in response to inv",
        idle_state.sock.send_message(message::Verack));
    }
    message::Verack => {}
    message::Addr(_) => {
      println!("Got addr, ignoring since we only support one peer for now.");
    }
    message::Block(block) => {
      println!("Received block: {:x}", block.header.hash());
      match idle_state.blockchain.add_block(block) {
         Err(e) => {
           println!("Failed to add block: {}", e);
         }
         _ => {}
      }
    },
    message::Headers(headers) => {
      for lone_header in headers.iter() {
        println!("Received header: {}, ignoring.", lone_header.header.hash());
      }
    },
    message::Inv(inv) => {
      println!("Received inv.");
      let sendmsg = message::GetData(inv);
      // Send
      consume_err("Warning: failed to send getdata in response to inv",
        idle_state.sock.send_message(sendmsg));
    }
    message::GetData(_) => {}
    message::NotFound(_) => {}
    message::GetBlocks(_) => {}
    message::GetHeaders(_) => {}
    message::Ping(nonce) => {
      consume_err("Warning: failed to send pong in response to ping",
        idle_state.sock.send_message(message::Pong(nonce)));
    }
    message::Pong(_) => {}
  }
}

#[cfg(test)]
mod tests {
  use bitcoin::network::constants::BitcoinTestnet;
  use bitcoin::network::listener::Listener;

  use user_data::{blockchain_path, utxo_set_path};
  use bitcoind::Bitcoind;

  #[test]
  fn test_bitcoind() {
    let bitcoind = Bitcoind::new("localhost", 1000,
                                 BitcoinTestnet,
                                 blockchain_path(BitcoinTestnet),
                                 utxo_set_path(BitcoinTestnet));
    assert_eq!(bitcoind.peer(), "localhost");
    assert_eq!(bitcoind.port(), 1000);

    let mut bitcoind = Bitcoind::new("localhost", 0,
                                     BitcoinTestnet,
                                     blockchain_path(BitcoinTestnet),
                                     utxo_set_path(BitcoinTestnet));
    assert!(bitcoind.listen().is_err());
  }
}
