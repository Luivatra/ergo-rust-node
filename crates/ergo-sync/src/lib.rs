//! # ergo-sync
//!
//! Block synchronization for the Ergo blockchain.
//!
//! This crate provides:
//! - Header-first synchronization strategy
//! - Block download scheduling
//! - Parallel block fetching
//! - Chain reorganization handling
//! - UTXO snapshot bootstrap support

mod download;
mod error;
mod protocol;
mod sync;

pub use download::{BlockDownloader, DownloadConfig, DownloadStats, DownloadTask};
pub use error::{SyncError, SyncResult};
pub use protocol::{SyncCommand, SyncEvent, SyncProtocol};
pub use sync::{SyncConfig, SyncState, Synchronizer};

/// Number of headers to request at once.
pub const HEADERS_BATCH_SIZE: usize = 100;

/// Maximum number of blocks to download in parallel.
/// Increased from 16 to 64 for better throughput while staying conservative.
pub const PARALLEL_DOWNLOADS: usize = 64;

/// Maximum blocks to keep in download queue.
pub const MAX_DOWNLOAD_QUEUE: usize = 1000;

#[cfg(test)]
mod tests {
    use ergo_chain_types::Header;
    use sigma_ser::ScorexSerializable;

    /// Test header ID computation for height 3132 (version 1 header).
    /// This header has a known ID from the explorer but sigma-rust computes a different one.
    #[test]
    fn test_header_3132_id_mismatch() {
        // Raw header bytes received from network for block 3132
        let data_hex = "01150290bbaf91ccd4dcf307cb9a5113eed67e12694ec9be277e8fa55fb5ebf6acc9d58eacf6108c9a166b0b76020e3323c6c2ccec5ec8f905ea46f5bcc58aac8001bf55fd587291172f458232a7f58b4b29469d72b8e304aafd68401f915b0c36144c15900826f6e2aac70cb50e541215b337d0d1674da6b491499944e686b41b0effe9cf80bb2dccb136ffd50a16f50a499e1c33d8ae1e8426bdc70b13a4d82275d057be2d04a70700a8a7bc1800000002ff03f4b981c59ccd5185fddcd949b8f5697341e60d808d2be0e3e09d2ec78bf4037427400e5292a177dc242631f78ab322b7845ad2b8491b016b7c36407c6a6d7600006677000084811affbd6a368ed127e55ccd7ea12ac55cf54cf1ed09c9b280a6d05c";
        let data = hex::decode(data_hex).unwrap();

        // Expected ID from Ergo explorer API
        let expected_id = "41c73753452a292442799bd884fbcc2a9b0f62d4cff7ad02ccd3dbe65791c908";

        // Parse the header
        let header = Header::scorex_parse_bytes(&data).unwrap();
        let computed_id = hex::encode(header.id.0.as_ref());

        println!("Header version: {}", header.version);
        println!("Header height: {}", header.height);
        println!("Parent ID: {}", hex::encode(header.parent_id.0.as_ref()));
        println!("Expected ID: {}", expected_id);
        println!("Computed ID: {}", computed_id);
        println!("Data length: {}", data.len());

        // Reserialize and compare
        let reserialized = header.scorex_serialize_bytes().unwrap();
        println!("Reserialized length: {}", reserialized.len());

        if data != reserialized {
            println!("\nMISMATCH between original and reserialized!");
            println!("Original:     {}", data_hex);
            println!("Reserialized: {}", hex::encode(&reserialized));

            // Find differences
            let min_len = std::cmp::min(data.len(), reserialized.len());
            for i in 0..min_len {
                if data[i] != reserialized[i] {
                    println!(
                        "First diff at byte {}: orig=0x{:02x}, reser=0x{:02x}",
                        i, data[i], reserialized[i]
                    );
                    break;
                }
            }
            if data.len() != reserialized.len() {
                println!(
                    "Length difference: orig={}, reser={}",
                    data.len(),
                    reserialized.len()
                );
            }
        }

        // The issue is that sigma-rust's BigInt serialization strips leading 0xFF bytes
        // which are sign-extension bytes for positive numbers with high bit set.
        // The original data has 0x1a (26 bytes) for d_bytes but reserialization produces 0x19 (25 bytes)

        // Workaround: compute the ID from the original bytes directly
        use blake2::digest::consts::U32;
        use blake2::{Blake2b, Digest};
        let mut hasher = Blake2b::<U32>::new();
        hasher.update(&data);
        let direct_hash = hasher.finalize();
        let direct_hash_hex = hex::encode(&direct_hash);
        println!("Direct hash of original bytes: {}", direct_hash_hex);

        // This should match the expected ID!
        assert_eq!(
            direct_hash_hex, expected_id,
            "Direct hash should match explorer ID"
        );

        // But sigma-rust's computed ID is based on reserialized bytes, so it differs
        assert_ne!(
            computed_id, expected_id,
            "sigma-rust's ID differs due to BigInt serialization issue"
        );

        // Verify we can parse the header
        assert_eq!(header.version, 1);
        assert_eq!(header.height, 3132);
    }
}
