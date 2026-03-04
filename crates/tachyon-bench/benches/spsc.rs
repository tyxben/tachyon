//! SPSC ring buffer benchmarks.

use arrayvec::ArrayVec;
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use tachyon_io::SpscQueue;

fn bench_push_pop(c: &mut Criterion) {
    let mut group = c.benchmark_group("spsc/push_pop");

    for &cap in &[1024, 4096, 65536] {
        group.bench_with_input(BenchmarkId::new("capacity", cap), &cap, |b, &cap| {
            let queue = SpscQueue::<u64>::new(cap);
            b.iter(|| {
                queue.try_push(black_box(42)).ok();
                black_box(queue.try_pop());
            });
        });
    }

    group.finish();
}

fn bench_burst(c: &mut Criterion) {
    let mut group = c.benchmark_group("spsc/burst");

    for &burst in &[16, 64, 256] {
        group.bench_with_input(BenchmarkId::new("size", burst), &burst, |b, &burst| {
            let queue = SpscQueue::<u64>::new(4096);
            b.iter(|| {
                // Push burst
                for i in 0..burst {
                    queue.try_push(black_box(i as u64)).ok();
                }
                // Drain burst
                let mut buf: ArrayVec<u64, 256> = ArrayVec::new();
                let n = queue.drain_batch(&mut buf);
                black_box(n);
            });
        });
    }

    group.finish();
}

fn bench_roundtrip_100(c: &mut Criterion) {
    c.bench_function("spsc/roundtrip_100", |b| {
        let queue = SpscQueue::<u64>::new(4096);
        b.iter(|| {
            for i in 0..100u64 {
                queue.try_push(i).ok();
            }
            for _ in 0..100 {
                black_box(queue.try_pop());
            }
        });
    });
}

criterion_group!(benches, bench_push_pop, bench_burst, bench_roundtrip_100);
criterion_main!(benches);
