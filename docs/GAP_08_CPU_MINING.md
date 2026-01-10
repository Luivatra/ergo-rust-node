# GAP_08: Internal CPU Mining

## Status: IMPLEMENTED
## Priority: LOW
## Effort: Medium
## Category: Mining

---

## Description

The Rust Ergo node supports external miners (via the mining API) but lacks internal CPU mining capability. Internal mining allows the node to mine blocks directly without requiring external mining software, which is useful for testnet, development, and small-scale mining operations.

---

## Implementation Summary

Internal CPU mining has been fully implemented with the following components:

### Files Created/Modified

| File | Description |
|------|-------------|
| `crates/ergo-mining/src/solver.rs` | Autolykos v2 PoW solver with batch processing |
| `crates/ergo-mining/src/worker.rs` | Mining worker threads with WorkerPool management |
| `crates/ergo-mining/src/miner.rs` | Extended with internal mining coordination |
| `crates/ergo-mining/src/lib.rs` | Exports for new modules |
| `crates/ergo-mining/Cargo.toml` | Added num_cpus, num-bigint, typenum dependencies |
| `crates/ergo-node/src/config.rs` | Added internal mining configuration |
| `crates/ergo-node/src/main.rs` | Added --internal-mining and --mining-threads CLI args |
| `crates/ergo-node/src/node.rs` | Integrated internal mining startup |

### Key Features

1. **Autolykos v2 Solver** (`solver.rs`):
   - Blake2b-based PoW algorithm
   - k=32 elements per nonce attempt
   - Height-dependent N parameter
   - Batch processing with cancellation support
   - Solution verification

2. **Mining Workers** (`worker.rs`):
   - Multi-threaded worker pool
   - Watch channel for task broadcasting
   - MPSC channel for solution reporting
   - Hash rate tracking
   - Graceful shutdown

3. **Mining Coordinator** (`miner.rs`):
   - `start_internal_mining()` async method
   - Block candidate generation
   - Solution handling
   - Statistics tracking

4. **Configuration**:
   - `--internal-mining` CLI flag
   - `--mining-threads <N>` CLI option
   - TOML config support

### Usage

```bash
# Start node with internal mining (4 threads)
ergo-node --internal-mining --mining-threads 4 --config ergo-node.toml
```

```toml
# ergo-node.toml
[mining]
enabled = true
internal = true
threads = 4
reward_address = "9f..."
```

---

## Previous State (Rust)

### What Existed

In `crates/ergo-mining/`:
- **Block candidate generation** - Complete (`candidate.rs`)
- **Coinbase creation** - Complete with EIP-27 (`coinbase.rs`)
- **Transaction selection** - Fee-based ordering
- **External miner protocol** - Framework in place (`miner.rs`)
- **Mining API** - GET/POST endpoints for candidates and solutions

### What Was Missing (Now Implemented)

1. ~~**Autolykos v2 PoW solver**~~ - Implemented in `solver.rs`
2. ~~**Mining threads**~~ - Implemented in `worker.rs`
3. ~~**Nonce iteration**~~ - Batch processing in solver
4. ~~**Table generation**~~ - Not needed for CPU mining (on-the-fly calculation)
5. ~~**Solution submission**~~ - Integrated in miner coordination

---

## Scala Reference

### Autolykos v2 Mining

```scala
// File: src/main/scala/org/ergoplatform/mining/AutolykosPowScheme.scala

class AutolykosPowScheme(k: Int, n: Int) {
  val NBase: Int = 26  // Initial n parameter (2^26 elements)
  
  def prove(
    header: HeaderWithoutPow,
    sk: BigInt,  // Secret key (for PoW hit calculation)
    message: Array[Byte]
  ): Option[AutolykosSolution] = {
    
    val nonce = generateNonce()
    val indexes = generateIndexes(message, nonce, n)
    
    // Sum of elements at indexes must be less than target
    val elements = indexes.map(i => genElement(message, i))
    val sum = elements.fold(BigInt(0))(_ + _)
    
    if (sum < header.requiredDifficulty.value) {
      Some(AutolykosSolution(nonce, indexes))
    } else {
      None
    }
  }
  
  private def genElement(message: Array[Byte], index: Int): BigInt = {
    // Blake2b256(message || index) -> BigInt
    val input = message ++ Ints.toByteArray(index)
    val hash = Blake2b256(input)
    BigInt(1, hash)
  }
  
  private def generateIndexes(message: Array[Byte], nonce: Array[Byte], n: Int): Seq[Int] = {
    // Generate k random indexes in range [0, n)
    val seed = Blake2b256(message ++ nonce)
    (0 until k).map { i =>
      val indexHash = Blake2b256(seed ++ Ints.toByteArray(i))
      BigInt(1, indexHash).mod(n).toInt
    }
  }
}
```

### Mining Thread

```scala
// File: src/main/scala/org/ergoplatform/mining/ErgoMiningThread.scala

class ErgoMiningThread(
  ergoMiner: ActorRef,
  powScheme: AutolykosPowScheme,
  sk: BigInt
) extends Thread {
  
  @volatile private var mining: Boolean = true
  @volatile private var candidate: Option[CandidateBlock] = None
  
  override def run(): Unit = {
    while (mining) {
      candidate.foreach { c =>
        val startNonce = Random.nextLong()
        val batchSize = 10000
        
        (0 until batchSize).foreach { i =>
          val nonce = startNonce + i
          powScheme.prove(c.header, sk, c.message, nonce) match {
            case Some(solution) =>
              ergoMiner ! FoundSolution(c, solution)
              return
            case None =>
          }
        }
      }
      
      Thread.sleep(10)  // Yield briefly
    }
  }
  
  def updateCandidate(c: CandidateBlock): Unit = {
    candidate = Some(c)
  }
  
  def stopMining(): Unit = {
    mining = false
  }
}
```

### Mining Coordinator

```scala
// File: src/main/scala/org/ergoplatform/mining/ErgoMiner.scala

class ErgoMiner(
  settings: ErgoSettings,
  nodeViewHolder: ActorRef,
  powScheme: AutolykosPowScheme
) extends Actor {
  
  private var miningThreads: Seq[ErgoMiningThread] = Seq.empty
  private var currentCandidate: Option[CandidateBlock] = None
  
  override def receive: Receive = {
    case StartMining =>
      if (settings.mining.useExternalMiner) {
        // External miner mode - just generate candidates
      } else {
        // Internal mining - start threads
        miningThreads = (0 until settings.mining.miningThreads).map { _ =>
          val thread = new ErgoMiningThread(self, powScheme, sk)
          thread.start()
          thread
        }
      }
      
    case GenerateCandidate =>
      val candidate = generateCandidate()
      currentCandidate = Some(candidate)
      miningThreads.foreach(_.updateCandidate(candidate))
      
    case FoundSolution(candidate, solution) =>
      if (currentCandidate.contains(candidate)) {
        val block = assembleBlock(candidate, solution)
        nodeViewHolder ! LocallyGeneratedBlock(block)
        
        // Generate next candidate
        self ! GenerateCandidate
      }
      
    case StopMining =>
      miningThreads.foreach(_.stopMining())
      miningThreads = Seq.empty
  }
}
```

---

## Impact

### Use Cases for Internal Mining

1. **Testnet mining** - Easy setup for testnet nodes
2. **Development** - Test mining workflows without external tools
3. **Solo mining** - Small-scale mining without pool software
4. **Education** - Understanding PoW algorithms

### Current Workaround

Users must run external mining software like:
- Ergo Reference Miner
- GPU mining software

This adds complexity for simple use cases.

---

## Implementation Plan

### Phase 1: Autolykos v2 Solver (3 days)

1. **Implement PoW solver** in `crates/ergo-mining/src/autolykos.rs`:
   ```rust
   use blake2::Blake2b256;
   use num_bigint::BigUint;
   
   pub struct AutolykosSolver {
       k: usize,  // Number of elements (32)
       n: usize,  // Table size (2^26 initially)
   }
   
   impl AutolykosSolver {
       pub const K: usize = 32;
       pub const N_BASE: u32 = 26;  // 2^26
       
       pub fn new(height: u32) -> Self {
           let n = Self::calculate_n(height);
           Self { k: Self::K, n }
       }
       
       /// Calculate n parameter based on block height
       fn calculate_n(height: u32) -> usize {
           // n grows by 5% every 50,144 blocks after height 614,400
           if height < 614_400 {
               1 << Self::N_BASE
           } else {
               let epochs = (height - 614_400) / 50_144;
               let growth = 1.05_f64.powi(epochs as i32);
               ((1 << Self::N_BASE) as f64 * growth).min(1 << 30) as usize
           }
       }
       
       /// Attempt to find a valid solution
       pub fn try_nonce(
           &self,
           header_bytes: &[u8],
           nonce: u64,
           target: &BigUint,
       ) -> Option<AutolykosSolution> {
           let nonce_bytes = nonce.to_be_bytes();
           
           // Generate k indexes
           let indexes = self.generate_indexes(header_bytes, &nonce_bytes);
           
           // Calculate sum of elements
           let sum = indexes.iter()
               .map(|&idx| self.gen_element(header_bytes, idx))
               .fold(BigUint::ZERO, |acc, e| acc + e);
           
           // Check if sum < target
           if sum < *target {
               Some(AutolykosSolution {
                   nonce,
                   d: sum,
               })
           } else {
               None
           }
       }
       
       fn gen_element(&self, msg: &[u8], index: u32) -> BigUint {
           let mut hasher = Blake2b256::new();
           hasher.update(msg);
           hasher.update(&index.to_be_bytes());
           let hash = hasher.finalize();
           BigUint::from_bytes_be(&hash)
       }
       
       fn generate_indexes(&self, msg: &[u8], nonce: &[u8]) -> Vec<u32> {
           let mut hasher = Blake2b256::new();
           hasher.update(msg);
           hasher.update(nonce);
           let seed = hasher.finalize();
           
           (0..self.k)
               .map(|i| {
                   let mut h = Blake2b256::new();
                   h.update(&seed);
                   h.update(&(i as u32).to_be_bytes());
                   let hash = h.finalize();
                   let val = BigUint::from_bytes_be(&hash);
                   (val % self.n).to_u32_digits()[0]
               })
               .collect()
       }
   }
   ```

### Phase 2: Mining Thread (2 days)

2. **Implement mining thread** in `crates/ergo-mining/src/worker.rs`:
   ```rust
   use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
   use std::sync::Arc;
   use tokio::sync::mpsc;
   
   pub struct MiningWorker {
       solver: AutolykosSolver,
       running: Arc<AtomicBool>,
       hash_count: Arc<AtomicU64>,
       solution_tx: mpsc::Sender<FoundSolution>,
   }
   
   pub struct MiningTask {
       pub header_bytes: Vec<u8>,
       pub target: BigUint,
       pub start_nonce: u64,
   }
   
   pub struct FoundSolution {
       pub nonce: u64,
       pub solution: AutolykosSolution,
   }
   
   impl MiningWorker {
       pub fn new(
           height: u32,
           solution_tx: mpsc::Sender<FoundSolution>,
       ) -> Self {
           Self {
               solver: AutolykosSolver::new(height),
               running: Arc::new(AtomicBool::new(false)),
               hash_count: Arc::new(AtomicU64::new(0)),
               solution_tx,
           }
       }
       
       pub async fn mine(&self, task: MiningTask) {
           self.running.store(true, Ordering::SeqCst);
           let batch_size = 10_000u64;
           let mut nonce = task.start_nonce;
           
           while self.running.load(Ordering::SeqCst) {
               for _ in 0..batch_size {
                   if let Some(solution) = self.solver.try_nonce(
                       &task.header_bytes,
                       nonce,
                       &task.target,
                   ) {
                       let _ = self.solution_tx.send(FoundSolution {
                           nonce,
                           solution,
                       }).await;
                       self.running.store(false, Ordering::SeqCst);
                       return;
                   }
                   nonce = nonce.wrapping_add(1);
               }
               
               self.hash_count.fetch_add(batch_size, Ordering::Relaxed);
               tokio::task::yield_now().await;
           }
       }
       
       pub fn stop(&self) {
           self.running.store(false, Ordering::SeqCst);
       }
       
       pub fn hash_rate(&self) -> u64 {
           self.hash_count.swap(0, Ordering::Relaxed)
       }
   }
   ```

### Phase 3: Mining Coordinator (2 days)

3. **Implement mining coordinator** in `crates/ergo-mining/src/miner.rs`:
   ```rust
   use tokio::sync::{mpsc, watch};
   
   pub struct InternalMiner {
       config: MiningConfig,
       workers: Vec<JoinHandle<()>>,
       candidate_tx: watch::Sender<Option<MiningTask>>,
       solution_rx: mpsc::Receiver<FoundSolution>,
       running: Arc<AtomicBool>,
   }
   
   impl InternalMiner {
       pub fn new(config: MiningConfig) -> Self {
           let (candidate_tx, _) = watch::channel(None);
           let (solution_tx, solution_rx) = mpsc::channel(10);
           
           Self {
               config,
               workers: Vec::new(),
               candidate_tx,
               solution_rx,
               running: Arc::new(AtomicBool::new(false)),
           }
       }
       
       pub async fn start(&mut self, num_threads: usize) {
           self.running.store(true, Ordering::SeqCst);
           
           for i in 0..num_threads {
               let candidate_rx = self.candidate_tx.subscribe();
               let solution_tx = self.solution_tx.clone();
               let running = self.running.clone();
               
               let handle = tokio::spawn(async move {
                   let mut worker = MiningWorker::new(0, solution_tx);
                   
                   loop {
                       if !running.load(Ordering::SeqCst) {
                           break;
                       }
                       
                       // Wait for new candidate
                       let task = {
                           let task = candidate_rx.borrow().clone();
                           task
                       };
                       
                       if let Some(mut task) = task {
                           // Offset nonce by thread ID to avoid overlap
                           task.start_nonce = task.start_nonce.wrapping_add(
                               i as u64 * u64::MAX / num_threads as u64
                           );
                           worker.mine(task).await;
                       }
                       
                       tokio::time::sleep(Duration::from_millis(100)).await;
                   }
               });
               
               self.workers.push(handle);
           }
       }
       
       pub fn update_candidate(&self, candidate: BlockCandidate) {
           let task = MiningTask {
               header_bytes: candidate.header_bytes(),
               target: candidate.target(),
               start_nonce: rand::random(),
           };
           let _ = self.candidate_tx.send(Some(task));
       }
       
       pub async fn wait_for_solution(&mut self) -> Option<FoundSolution> {
           self.solution_rx.recv().await
       }
       
       pub fn stop(&self) {
           self.running.store(false, Ordering::SeqCst);
       }
   }
   ```

### Phase 4: Integration (1 day)

4. **Integrate with node** in `crates/ergo-node/src/node.rs`:
   ```rust
   impl Node {
       async fn start_internal_mining(&mut self) {
           if !self.config.mining.enabled {
               return;
           }
           
           if self.config.mining.use_external_miner {
               tracing::info!("Using external miner mode");
               return;
           }
           
           tracing::info!(
               threads = self.config.mining.threads,
               "Starting internal CPU mining"
           );
           
           let mut miner = InternalMiner::new(self.config.mining.clone());
           miner.start(self.config.mining.threads).await;
           
           // Mining loop
           loop {
               // Generate candidate
               let candidate = self.generate_candidate().await?;
               miner.update_candidate(candidate);
               
               // Wait for solution or new block
               tokio::select! {
                   solution = miner.wait_for_solution() => {
                       if let Some(sol) = solution {
                           self.submit_solution(sol).await?;
                       }
                   }
                   _ = self.new_block_signal.recv() => {
                       // New block received, regenerate candidate
                       continue;
                   }
               }
           }
       }
   }
   ```

---

## Files to Create/Modify

### New Files
```
crates/ergo-mining/src/autolykos.rs   # Autolykos v2 solver
crates/ergo-mining/src/worker.rs      # Mining worker thread
```

### Modified Files
```
crates/ergo-mining/src/miner.rs       # Add InternalMiner
crates/ergo-mining/src/lib.rs         # Export new modules
crates/ergo-node/src/node.rs          # Integrate mining
crates/ergo-node/src/config.rs        # Add mining threads config
```

---

## Estimated Effort

| Task | Time |
|------|------|
| Autolykos v2 solver | 3 days |
| Mining thread | 2 days |
| Mining coordinator | 2 days |
| Node integration | 1 day |
| Testing | 2 days |
| **Total** | **10 days** |

---

## Dependencies

- Block candidate generation (exists)
- Autolykos verification (exists in ergo-consensus)
- Blake2b hashing (available)

---

## Test Strategy

### Unit Tests

```rust
#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_autolykos_verification_matches_solver() {
        let solver = AutolykosSolver::new(100);
        let header = create_test_header();
        let target = BigUint::from(u64::MAX); // Easy target
        
        // Find a solution
        for nonce in 0..1_000_000 {
            if let Some(solution) = solver.try_nonce(&header, nonce, &target) {
                // Verify using consensus module
                let verified = ergo_consensus::verify_pow(&header, &solution);
                assert!(verified.is_ok());
                return;
            }
        }
        
        panic!("Should find solution with easy target");
    }
    
    #[test]
    fn test_n_parameter_growth() {
        // Before growth starts
        assert_eq!(AutolykosSolver::calculate_n(0), 1 << 26);
        assert_eq!(AutolykosSolver::calculate_n(614_399), 1 << 26);
        
        // After one epoch
        let n_one_epoch = AutolykosSolver::calculate_n(614_400 + 50_144);
        assert!(n_one_epoch > 1 << 26);
        assert!(n_one_epoch < 1 << 27);
    }
}
```

### Integration Tests

```rust
#[tokio::test]
async fn test_internal_mining_finds_block() {
    let mut miner = InternalMiner::new(MiningConfig {
        threads: 4,
        ..Default::default()
    });
    
    miner.start(4).await;
    
    let easy_candidate = create_candidate_with_low_difficulty();
    miner.update_candidate(easy_candidate);
    
    let solution = tokio::time::timeout(
        Duration::from_secs(60),
        miner.wait_for_solution()
    ).await;
    
    assert!(solution.is_ok());
    assert!(solution.unwrap().is_some());
}
```

---

## Success Metrics

1. **Correctness**: Mined blocks pass verification
2. **Performance**: Reasonable hash rate for CPU mining
3. **Stability**: No crashes during extended mining
4. **Compatibility**: Solutions accepted by network

---

## Configuration

```toml
[mining]
enabled = true
use_external_miner = false  # Use internal miner
threads = 4                 # Number of mining threads
reward_address = "9f..."    # Miner reward address
```

---

## Scala File References

| Feature | Scala File |
|---------|------------|
| Autolykos PoW | `ergo-core/src/main/scala/org/ergoplatform/mining/AutolykosPowScheme.scala` |
| Mining thread | `src/main/scala/org/ergoplatform/mining/ErgoMiningThread.scala` |
| Miner actor | `src/main/scala/org/ergoplatform/mining/ErgoMiner.scala` |
| Candidate generation | `src/main/scala/org/ergoplatform/mining/CandidateGenerator.scala` |
