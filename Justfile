set positional-arguments
alias t := test
alias la := lint-all
alias l := lint-native
alias lint := lint-native
alias f := fmt-native-fix
alias b := build-native
alias h := hack

exclude_members := "--exclude celo-registry --exclude execution-fixture"

# default recipe to display help information
default:
  @just --list

# Test for the native target with all features.
test:
  cargo nextest run --workspace --all-features {{exclude_members}}

# Runs benchmarks
benches:
  cargo bench --no-run --workspace --features test-utils {{exclude_members}}

# Lint the workspace for all available targets
lint-all: lint-native lint-docs

# Runs `cargo hack check` against the workspace
hack:
  cargo hack check --no-default-features --no-dev-deps {{exclude_members}}

# Fixes the formatting of the workspace
fmt-native-fix:
  cargo +nightly fmt --all

# Check the formatting of the workspace
fmt-native-check:
  cargo +nightly fmt --all -- --check

# Lint the workspace
lint-native: fmt-native-check lint-docs
  cargo clippy --workspace --all-features --all-targets {{exclude_members}} -- -D warnings

# Lint the Rust documentation
lint-docs:
  RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items {{exclude_members}}

# Build for the native target
build-native *args='':
  cargo build --workspace {{exclude_members}} $@

# Check for unused dependencies in the crate graph.
check-udeps:
  cargo +nightly udeps --workspace --all-features --all-targets

# Download resources/g1.point if it doesn't exist.
download-srs:
  #!/usr/bin/env bash
  if [ ! -f "resources/g1.point" ]; then
      echo "Downloading SRS G1 points to resources/g1.point ..."
      mkdir -p resources
      curl -o resources/g1.point -L https://github.com/Layr-Labs/eigenda-proxy/raw/refs/heads/main/resources/g1.point || { echo "Error: Failed to download SRS G1 points."; exit 1; }
  else
      echo "SRS file resources/g1.point already exists, skipping download"
  fi

# 🚀 Enhanced Performance & Security Commands

# Run performance monitoring demo
demo-performance:
  cargo run --example performance-demo

# Run comprehensive security audit
security-audit:
  cargo audit
  cargo geiger --format GitHubMarkdown
  cargo deny check

# Run performance benchmarks with monitoring
bench-monitored:
  cargo bench --workspace --features test-utils {{exclude_members}}
  @echo "📊 Benchmark results available in target/criterion/"

# Generate comprehensive project report
generate-report:
  @echo "🚀 Generating Celo-Kona Project Report..."
  @echo "# Celo-Kona Project Report" > PROJECT_REPORT.md
  @echo "Generated on: $(date)" >> PROJECT_REPORT.md
  @echo "" >> PROJECT_REPORT.md
  @echo "## 📊 Code Statistics" >> PROJECT_REPORT.md
  tokei --output json | jq -r '.Total | "- Total Lines: " + (.code | tostring) + "\n- Comment Lines: " + (.comments | tostring) + "\n- Blank Lines: " + (.blanks | tostring)' >> PROJECT_REPORT.md || echo "Install tokei for code statistics"
  @echo "" >> PROJECT_REPORT.md
  @echo "## 🔒 Security Status" >> PROJECT_REPORT.md
  cargo audit --format json | jq -r '.vulnerabilities | length | "- Vulnerabilities Found: " + tostring' >> PROJECT_REPORT.md || echo "- Security audit completed" >> PROJECT_REPORT.md
  @echo "" >> PROJECT_REPORT.md
  @echo "## 🏗️ Build Status" >> PROJECT_REPORT.md
  cargo check --workspace {{exclude_members}} && echo "- ✅ Build: Passing" >> PROJECT_REPORT.md || echo "- ❌ Build: Failing" >> PROJECT_REPORT.md
  @echo "📄 Report generated: PROJECT_REPORT.md"

# Start monitoring server
start-monitoring:
  @echo "🚀 Starting Celo-Kona with Performance Monitoring..."
  @echo "📊 Prometheus metrics: http://localhost:9090/metrics"
  @echo "🔍 Performance demo: just demo-performance"
  cargo run --bin celo-host -- --enable-monitoring

# Clean all artifacts including monitoring data
clean-all: 
  cargo clean
  rm -rf target/
  rm -f PROJECT_REPORT.md
  rm -f *.log
  @echo "🧹 All artifacts cleaned"

# Setup development environment with monitoring tools
setup-dev:
  @echo "🔧 Setting up development environment..."
  cargo install tokei cargo-audit cargo-deny cargo-geiger cargo-criterion
  @echo "✅ Development tools installed"
  @echo "💡 Run 'just demo-performance' to test monitoring"

# Quick health check of the entire system
health-check:
  @echo "🏥 Performing Celo-Kona Health Check..."
  @echo "1. 🦀 Checking Rust version..."
  rustc --version
  @echo "2. 📦 Checking dependencies..."
  cargo check --workspace {{exclude_members}} > /dev/null && echo "   ✅ Dependencies OK" || echo "   ❌ Dependency issues found"
  @echo "3. 🧪 Running quick tests..."
  cargo test --workspace --lib {{exclude_members}} > /dev/null && echo "   ✅ Tests passing" || echo "   ❌ Test failures found"
  @echo "4. 🔒 Security check..."
  cargo audit > /dev/null && echo "   ✅ No known vulnerabilities" || echo "   ⚠️  Security issues found"
  @echo "5. 📊 Performance module..."
  cargo check -p celo-performance-monitor > /dev/null && echo "   ✅ Performance monitor OK" || echo "   ❌ Performance monitor issues"
  @echo "🎉 Health check completed!"