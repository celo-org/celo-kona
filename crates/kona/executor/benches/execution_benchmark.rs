use criterion::{black_box, criterion_group, criterion_main, Criterion};
use alloy_primitives::{B256, U256};
use alloy_consensus::Header;
use celo_executor::util::{encode_holocene_eip_1559_params, decode_holocene_eip_1559_params};
use celo_alloy_rpc_types_engine::CeloPayloadAttributes;
use celo_genesis::CeloRollupConfig;
use kona_genesis::RollupConfig;
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use alloy_rpc_types_engine::PayloadAttributes;
use alloy_primitives::{B64, b64};

fn create_mock_config() -> CeloRollupConfig {
    CeloRollupConfig {
        op_rollup_config: RollupConfig {
            chain_op_config: kona_genesis::BaseFeeConfig {
                eip1559_denominator: 32,
                eip1559_elasticity: 64,
                eip1559_denominator_canyon: 32,
            },
            ..Default::default()
        },
    }
}

fn create_mock_payload_attributes() -> CeloPayloadAttributes {
    CeloPayloadAttributes {
        op_payload_attributes: OpPayloadAttributes {
            payload_attributes: PayloadAttributes {
                timestamp: 1234567890,
                prev_randao: B256::ZERO,
                suggested_fee_recipient: Default::default(),
                withdrawals: Default::default(),
                parent_beacon_block_root: Default::default(),
            },
            transactions: None,
            no_tx_pool: None,
            gas_limit: None,
            eip_1559_params: Some(b64!("0000004000000060")),
        },
    }
}

fn create_mock_header() -> Header {
    Header {
        number: 1000,
        timestamp: 1234567890,
        gas_limit: 30_000_000,
        gas_used: 15_000_000,
        base_fee_per_gas: Some(25_000_000_000),
        extra_data: vec![0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x60].into(),
        ..Default::default()
    }
}

fn bench_encode_holocene_params(c: &mut Criterion) {
    let config = create_mock_config();
    let attrs = create_mock_payload_attributes();
    
    c.bench_function("encode_holocene_eip_1559_params", |b| {
        b.iter(|| {
            black_box(encode_holocene_eip_1559_params(
                black_box(&config),
                black_box(&attrs)
            ))
        })
    });
}

fn bench_decode_holocene_params(c: &mut Criterion) {
    let header = create_mock_header();
    
    c.bench_function("decode_holocene_eip_1559_params", |b| {
        b.iter(|| {
            black_box(decode_holocene_eip_1559_params(black_box(&header)))
        })
    });
}

fn bench_receipt_processing(c: &mut Criterion) {
    use celo_alloy_consensus::{CeloReceiptEnvelope, CeloTxReceipt};
    use alloy_consensus::TxReceipt;
    
    // Create mock receipts
    let mut receipts = Vec::new();
    for i in 0..1000 {
        let receipt = CeloReceiptEnvelope::Deposit(celo_alloy_consensus::CeloDepositReceipt {
            receipt: CeloTxReceipt {
                inner: TxReceipt {
                    status: alloy_consensus::Eip658Value::Eip658(true),
                    cumulative_gas_used: 21000 * (i + 1),
                    logs: vec![],
                },
                deposit_nonce: Some(i),
                deposit_receipt_version: Some(1),
            },
        });
        receipts.push(receipt);
    }
    
    c.bench_function("receipt_processing_optimized", |b| {
        b.iter(|| {
            // Simulate the optimized receipt processing
            let mut processed_receipts = Vec::with_capacity(receipts.len());
            for receipt in receipts.iter() {
                let processed = match receipt {
                    CeloReceiptEnvelope::Deposit(mut deposit_receipt) => {
                        deposit_receipt.receipt.deposit_nonce = None;
                        CeloReceiptEnvelope::Deposit(deposit_receipt)
                    }
                    _ => receipt.clone(),
                };
                processed_receipts.push(processed);
            }
            black_box(processed_receipts)
        })
    });
    
    c.bench_function("receipt_processing_original", |b| {
        b.iter(|| {
            let processed: Vec<_> = receipts
                .iter()
                .cloned()
                .map(|receipt| match receipt {
                    CeloReceiptEnvelope::Deposit(mut deposit_receipt) => {
                        deposit_receipt.receipt.deposit_nonce = None;
                        CeloReceiptEnvelope::Deposit(deposit_receipt)
                    }
                    _ => receipt,
                })
                .collect();
            black_box(processed)
        })
    });
}

criterion_group!(
    benches,
    bench_encode_holocene_params,
    bench_decode_holocene_params,
    bench_receipt_processing
);
criterion_main!(benches);