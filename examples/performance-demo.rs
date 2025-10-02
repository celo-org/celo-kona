//! # 🚀 Celo-Kona Performance Monitoring Demo
//!
//! This example demonstrates the advanced performance monitoring capabilities
//! of the enhanced Celo-Kona implementation.
//!
//! ## Features Demonstrated
//!
//! - Real-time system metrics collection
//! - Custom performance profiling
//! - Memory leak detection
//! - Prometheus metrics export
//! - Performance regression detection
//!
//! ## Usage
//!
//! ```bash
//! cargo run --example performance-demo
//! ```
//!
//! Then visit http://localhost:9090/metrics to see live Prometheus metrics.

use anyhow::Result;
use celo_performance_monitor::{
    PerformanceMonitor, ProfileScope, MonitorConfig,
    profile, time_operation
};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .init();

    info!("🚀 Starting Celo-Kona Performance Monitoring Demo");

    // Create custom configuration
    let config = MonitorConfig {
        enable_memory_monitoring: true,
        enable_cpu_monitoring: true,
        enable_network_monitoring: true,
        monitoring_interval_ms: 500, // More frequent updates for demo
        max_metrics_history: 1000,
        enable_prometheus_export: true,
        prometheus_port: 9090,
        enable_regression_detection: true,
        regression_threshold: 15.0, // 15% threshold for demo
    };

    // Initialize performance monitor
    let monitor = PerformanceMonitor::with_config(config).await?;
    info!("📊 Performance monitor initialized");

    // Start monitoring
    monitor.start_monitoring().await?;
    info!("🔍 Monitoring started - visit http://localhost:9090/metrics for live data");

    // Demonstrate various monitoring features
    demo_basic_profiling(&monitor).await?;
    demo_custom_metrics(&monitor).await?;
    demo_memory_intensive_operation(&monitor).await?;
    demo_cpu_intensive_operation(&monitor).await?;
    demo_concurrent_operations(&monitor).await?;

    // Show performance summary
    let summary = monitor.get_performance_summary().await?;
    info!("📊 Performance Summary:");
    info!("  ⏱️  Uptime: {:?}", summary.uptime);
    info!("  🖥️  Average CPU: {:.2}%", summary.average_cpu_usage);
    info!("  🧠 Current Memory: {:.2} MB", summary.current_memory_usage as f64 / 1024.0 / 1024.0);
    info!("  📈 Metrics Collected: {}", summary.total_metrics_collected);
    info!("  🎯 Custom Metrics: {}", summary.custom_metrics_count);

    info!("✅ Demo completed! Monitor will continue running...");
    info!("💡 Press Ctrl+C to stop");

    // Keep running to show live metrics
    loop {
        sleep(Duration::from_secs(10)).await;
        
        // Simulate some background activity
        {
            profile!("background_activity");
            simulate_work().await;
        }
        
        // Add a custom metric
        monitor.add_custom_metric("demo_heartbeat", 1.0).await;
    }
}

/// Demonstrate basic profiling capabilities
async fn demo_basic_profiling(monitor: &PerformanceMonitor) -> Result<()> {
    info!("🎯 Demo: Basic Profiling");

    // Profile a simple operation
    {
        let _scope = ProfileScope::new("simple_operation");
        sleep(Duration::from_millis(100)).await;
        info!("  ✅ Simple operation completed");
    }

    // Profile nested operations
    {
        let _outer_scope = ProfileScope::new("outer_operation");
        
        for i in 0..3 {
            let _inner_scope = ProfileScope::new(format!("inner_operation_{}", i));
            sleep(Duration::from_millis(50)).await;
        }
        
        info!("  ✅ Nested operations completed");
    }

    // Use the time_operation macro
    let result = time_operation!(monitor, "timed_calculation", {
        // Simulate some calculation
        let mut sum = 0u64;
        for i in 0..1000000 {
            sum += i;
        }
        sum
    });

    info!("  🧮 Calculation result: {}", result);
    
    Ok(())
}

/// Demonstrate custom metrics
async fn demo_custom_metrics(monitor: &PerformanceMonitor) -> Result<()> {
    info!("📊 Demo: Custom Metrics");

    // Add various custom metrics
    monitor.add_custom_metric("demo_counter", 42.0).await;
    monitor.add_custom_metric("demo_temperature", 65.5).await;
    monitor.add_custom_metric("demo_success_rate", 98.7).await;

    // Simulate metric updates over time
    for i in 0..10 {
        monitor.add_custom_metric("demo_dynamic_value", (i as f64 * 1.5) + 10.0).await;
        sleep(Duration::from_millis(100)).await;
    }

    info!("  ✅ Custom metrics added");
    Ok(())
}

/// Demonstrate memory-intensive operations
async fn demo_memory_intensive_operation(monitor: &PerformanceMonitor) -> Result<()> {
    info!("🧠 Demo: Memory Intensive Operation");

    {
        profile!("memory_allocation");
        
        // Allocate and deallocate memory to show memory tracking
        let mut vectors = Vec::new();
        
        for i in 0..100 {
            let vec = vec![0u8; 1024 * 1024]; // 1MB allocation
            vectors.push(vec);
            
            if i % 10 == 0 {
                monitor.add_custom_metric("allocated_mb", (i + 1) as f64).await;
                sleep(Duration::from_millis(10)).await;
            }
        }
        
        info!("  📈 Allocated {} MB", vectors.len());
        
        // Gradual deallocation
        while !vectors.is_empty() {
            vectors.pop();
            if vectors.len() % 20 == 0 {
                monitor.add_custom_metric("remaining_mb", vectors.len() as f64).await;
                sleep(Duration::from_millis(5)).await;
            }
        }
        
        info!("  📉 Memory deallocated");
    }

    Ok(())
}

/// Demonstrate CPU-intensive operations
async fn demo_cpu_intensive_operation(monitor: &PerformanceMonitor) -> Result<()> {
    info!("🖥️ Demo: CPU Intensive Operation");

    {
        profile!("cpu_intensive_calculation");
        
        // Simulate CPU-intensive work
        let result = time_operation!(monitor, "prime_calculation", {
            calculate_primes(10000)
        });
        
        info!("  🔢 Found {} prime numbers", result.len());
        monitor.add_custom_metric("primes_found", result.len() as f64).await;
    }

    // Parallel CPU work
    {
        profile!("parallel_cpu_work");
        
        let handles = (0..4).map(|i| {
            tokio::spawn(async move {
                let primes = calculate_primes(5000);
                info!("    Thread {} found {} primes", i, primes.len());
                primes.len()
            })
        }).collect::<Vec<_>>();
        
        let mut total_primes = 0;
        for handle in handles {
            total_primes += handle.await?;
        }
        
        monitor.add_custom_metric("parallel_primes_total", total_primes as f64).await;
        info!("  ⚡ Parallel calculation completed: {} total primes", total_primes);
    }

    Ok(())
}

/// Demonstrate concurrent operations monitoring
async fn demo_concurrent_operations(monitor: &PerformanceMonitor) -> Result<()> {
    info!("🔄 Demo: Concurrent Operations");

    let monitor_clone = monitor.clone();
    
    // Spawn multiple concurrent tasks
    let handles = (0..5).map(|task_id| {
        let monitor = monitor_clone.clone();
        tokio::spawn(async move {
            let _scope = ProfileScope::new(format!("concurrent_task_{}", task_id));
            
            // Simulate different types of work
            match task_id % 3 {
                0 => {
                    // I/O simulation
                    sleep(Duration::from_millis(200)).await;
                    monitor.add_custom_metric(format!("task_{}_io_ops", task_id), 15.0).await;
                }
                1 => {
                    // CPU simulation
                    let primes = calculate_primes(1000);
                    monitor.add_custom_metric(format!("task_{}_cpu_result", task_id), primes.len() as f64).await;
                }
                _ => {
                    // Mixed workload
                    sleep(Duration::from_millis(100)).await;
                    let _calc = (0..100000).fold(0u64, |acc, x| acc.wrapping_add(x));
                    monitor.add_custom_metric(format!("task_{}_mixed_work", task_id), 1.0).await;
                }
            }
            
            info!("    Task {} completed", task_id);
        })
    }).collect::<Vec<_>>();
    
    // Wait for all tasks to complete
    for handle in handles {
        handle.await?;
    }
    
    info!("  ✅ All concurrent tasks completed");
    Ok(())
}

/// Simple prime number calculation for CPU load simulation
fn calculate_primes(limit: usize) -> Vec<usize> {
    let mut primes = Vec::new();
    let mut is_prime = vec![true; limit + 1];
    
    for i in 2..=limit {
        if is_prime[i] {
            primes.push(i);
            let mut j = i * i;
            while j <= limit {
                is_prime[j] = false;
                j += i;
            }
        }
    }
    
    primes
}

/// Simulate some background work
async fn simulate_work() {
    let _scope = ProfileScope::new("background_simulation");
    
    // Light CPU work
    let _result = (0..10000).fold(0u64, |acc, x| acc.wrapping_add(x));
    
    // Short sleep to simulate I/O
    sleep(Duration::from_millis(10)).await;
}
