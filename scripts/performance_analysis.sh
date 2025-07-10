#!/bin/bash

# Performance Analysis Script for Celo Blockchain Client
# This script runs various performance-related checks and optimizations

set -e

echo "🚀 Celo Blockchain Client Performance Analysis"
echo "============================================="

# Check if required tools are installed
check_tool() {
    if ! command -v "$1" &> /dev/null; then
        echo "❌ $1 is not installed. Please install it first."
        exit 1
    fi
}

echo "📋 Checking required tools..."
check_tool "cargo"
check_tool "just"

# 1. Check for unused dependencies
echo ""
echo "🔍 Checking for unused dependencies..."
if command -v cargo-udeps &> /dev/null; then
    echo "Running cargo-udeps..."
    cargo +nightly udeps --workspace --all-features --all-targets || echo "⚠️ Some unused dependencies found"
else
    echo "⚠️ cargo-udeps not found. Install with: cargo install cargo-udeps"
fi

# 2. Build size analysis
echo ""
echo "📊 Analyzing build sizes..."
echo "Building release binaries..."
just build-native --release

echo ""
echo "Binary sizes:"
if [ -d "target/release" ]; then
    ls -la target/release/celo-* 2>/dev/null || echo "No celo binaries found"
    echo ""
    echo "Total binary size:"
    du -sh target/release/celo-* 2>/dev/null || echo "No binaries to measure"
else
    echo "No release build found"
fi

# 3. Build size comparison with different profiles
echo ""
echo "📈 Comparing build profiles..."

echo "Building with size optimization..."
just build-size
if [ -d "target/release-size" ]; then
    echo "Size-optimized binaries:"
    ls -la target/release-size/celo-* 2>/dev/null || echo "No size-optimized binaries found"
fi

# 4. Run benchmarks if available
echo ""
echo "⚡ Running performance benchmarks..."
if [ -f "crates/kona/executor/benches/execution_benchmark.rs" ]; then
    echo "Running executor benchmarks..."
    cd crates/kona/executor
    cargo bench
    cd - > /dev/null
else
    echo "No benchmarks found"
fi

# 5. Compilation time analysis
echo ""
echo "⏱️ Compilation time analysis..."
echo "Building with timing information..."
cargo build --workspace --timings

if [ -f "cargo-timing.html" ]; then
    echo "✅ Compilation timing report generated: cargo-timing.html"
else
    echo "⚠️ No timing report generated"
fi

# 6. Memory usage analysis (if available)
echo ""
echo "🧠 Memory usage analysis..."
if command -v valgrind &> /dev/null; then
    echo "Valgrind available for memory profiling"
    echo "Run: valgrind --tool=callgrind target/release/celo-host --help"
else
    echo "⚠️ Valgrind not available for memory profiling"
fi

# 7. Check for common performance anti-patterns
echo ""
echo "🔍 Checking for performance anti-patterns..."
echo "Searching for potential issues..."

# Check for String::new() that could be pre-allocated
string_news=$(find . -name "*.rs" -not -path "./target/*" -exec grep -l "String::new()" {} \; 2>/dev/null | wc -l)
echo "String::new() usage: $string_news files (consider pre-allocation)"

# Check for Vec::new() that could be pre-allocated
vec_news=$(find . -name "*.rs" -not -path "./target/*" -exec grep -l "Vec::new()" {} \; 2>/dev/null | wc -l)
echo "Vec::new() usage: $vec_news files (consider with_capacity)"

# Check for excessive cloning
clones=$(find . -name "*.rs" -not -path "./target/*" -exec grep -c "\.clone()" {} \; 2>/dev/null | awk '{sum += $1} END {print sum}')
echo "Clone operations: $clones total (review for necessity)"

# 8. Generate performance report
echo ""
echo "📄 Generating performance report..."
{
    echo "# Performance Analysis Report - $(date)"
    echo ""
    echo "## Build Sizes"
    echo "```"
    ls -la target/release/celo-* 2>/dev/null || echo "No binaries found"
    echo "```"
    echo ""
    echo "## Compilation Time"
    echo "See cargo-timing.html for detailed analysis"
    echo ""
    echo "## Performance Metrics"
    echo "- String::new() usage: $string_news files"
    echo "- Vec::new() usage: $vec_news files"
    echo "- Clone operations: $clones total"
    echo ""
    echo "## Recommendations"
    echo "1. Consider pre-allocating collections where size is known"
    echo "2. Review clone operations for necessity"
    echo "3. Use size-optimized builds for production"
    echo "4. Enable parallel features for better performance"
} > performance_report.md

echo "✅ Performance analysis complete!"
echo "📄 Report saved to: performance_report.md"
echo ""
echo "💡 Next steps:"
echo "1. Review unused dependencies with: cargo +nightly udeps"
echo "2. Run benchmarks with: just bench"
echo "3. Build size-optimized with: just build-size"
echo "4. Enable parallel features for better performance"