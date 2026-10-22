use criterion::{black_box, criterion_group, criterion_main, Criterion};
use rkyv::{Archive, Serialize as RkyvSerialize};
use rpi_plugin_sdk::binary::{self, BinaryOptions};
use rpi_plugin_sdk::zero_copy::{self, ZeroCopyOptions};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Archive, RkyvSerialize, Serialize, Deserialize)]
#[archive(check_bytes)]
struct ZeroCopyPayload {
    request_id: String,
    tool_name: String,
    arguments: String,
    flags: Vec<String>,
    bytes: u64,
}

fn sample() -> ZeroCopyPayload {
    ZeroCopyPayload {
        request_id: "req_01HZZZZZZZZZZZZZZZZZZZZZZ".into(),
        tool_name: "read".into(),
        arguments: r#"{"path":"src/agent_loop.rs","line_start":1,"line_end":420}"#.into(),
        flags: vec!["zero-copy".into(), "streaming".into(), "trusted".into()],
        bytes: 16384,
    }
}

fn bench_zero_copy(c: &mut Criterion) {
    let binary_options = BinaryOptions::default();
    let zero_copy_options = ZeroCopyOptions::default();
    let value = sample();
    let json_bytes = serde_json::to_vec(&value).unwrap();
    let binary_bytes = binary::encode(&value, &binary_options).unwrap();
    let zero_copy_bytes = zero_copy::encode(&value, &zero_copy_options).unwrap();
    eprintln!(
        "payload sizes: json={} bytes, cbor={} bytes, zero-copy={} bytes",
        json_bytes.len(),
        binary_bytes.len(),
        zero_copy_bytes.len()
    );

    let mut group = c.benchmark_group("serialization_comparison");
    group.bench_function("json_encode", |b| {
        b.iter(|| serde_json::to_vec(black_box(&value)).unwrap())
    });
    group.bench_function("cbor_encode", |b| {
        b.iter(|| binary::encode(black_box(&value), &binary_options).unwrap())
    });
    group.bench_function("zero_copy_encode", |b| {
        b.iter(|| zero_copy::encode(black_box(&value), &zero_copy_options).unwrap())
    });
    group.bench_function("json_decode", |b| {
        b.iter(|| serde_json::from_slice::<ZeroCopyPayload>(black_box(&json_bytes)).unwrap())
    });
    group.bench_function("cbor_decode", |b| {
        b.iter(|| {
            binary::decode::<ZeroCopyPayload>(black_box(&binary_bytes), &binary_options).unwrap()
        })
    });
    group.bench_function("zero_copy_validate_and_read", |b| {
        b.iter(|| {
            let archived =
                zero_copy::view::<ZeroCopyPayload>(black_box(&zero_copy_bytes), &zero_copy_options)
                    .unwrap();
            black_box((
                archived.bytes,
                archived.flags.len(),
                archived.tool_name.as_str(),
            ))
        })
    });
    let archived =
        zero_copy::view::<ZeroCopyPayload>(&zero_copy_bytes, &zero_copy_options).unwrap();
    group.bench_function("zero_copy_read_validated", |b| {
        b.iter(|| {
            black_box((
                archived.bytes,
                archived.flags.len(),
                archived.tool_name.as_str(),
            ))
        })
    });
    group.finish();
}

criterion_group!(benches, bench_zero_copy);
criterion_main!(benches);
