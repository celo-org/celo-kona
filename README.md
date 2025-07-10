# Celo-Kona

A high-performance Celo blockchain client built on the Kona framework.

## Performance Optimizations

This codebase includes comprehensive performance optimizations for improved build times, runtime performance, and memory efficiency.

📊 **[Performance Optimizations Guide](PERFORMANCE_OPTIMIZATIONS.md)** - Detailed guide on performance features and optimizations

### Quick Start

```bash
# Fast development builds
cargo build --profile dev-fast

# Size-optimized production builds
just build-size

# Maximum performance with parallel processing
just build-native-fast --features parallel

# Run performance analysis
just perf-analysis
```

### Performance Features

- **Optimized Build Profiles**: Fast compilation, size optimization, and LTO
- **Memory Optimizations**: Pre-allocated collections, stack allocation for hot paths
- **Parallel Processing**: Optional parallel transaction processing with Rayon
- **Benchmarking Suite**: Comprehensive performance benchmarks
- **Memory Profiling**: Built-in memory profiling support

For detailed information about performance optimizations, see [PERFORMANCE_OPTIMIZATIONS.md](PERFORMANCE_OPTIMIZATIONS.md).