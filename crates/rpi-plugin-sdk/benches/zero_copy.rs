use criterion::{black_box, criterion_group, criterion_main, Criterion};
use rkyv::{Archive, Serialize};
use rpi_plugin_sdk::zero_copy::{self, ZeroCopyOptions};

#[derive(Debug, Clone, Archive, Serialize)]
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
    let options = ZeroCopyOptions::default();
    let value = sample();
    let bytes = zero_copy::encode(&value, &options).unwrap();
    eprintln!("zero-copy archive size: {} bytes", bytes.len());

    let mut group = c.benchmark_group("zero_copy");
    group.bench_function("encode", |b| {
        b.iter(|| zero_copy::encode(black_box(&value), &options).unwrap())
    });
    group.bench_function("validate_and_read", |b| {
        b.iter(|| {
            let archived = zero_copy::view::<ZeroCopyPayload>(black_box(&bytes), &options).unwrap();
            black_box((
                archived.bytes,
                archived.flags.len(),
                archived.tool_name.as_str(),
            ))
        })
    });
    group.bench_function("read_validated_archive", |b| {
        let archived = zero_copy::view::<ZeroCopyPayload>(&bytes, &options).unwrap();
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
