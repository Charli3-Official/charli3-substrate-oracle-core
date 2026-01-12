# Charli3 Substrate Oracle Core

Core oracle types, aggregation algorithms, and price providers for Charli3. This library provides shared functionality for the Charli3 oracle system, supporting both standard environments and `no_std` builds for Substrate runtimes.

## Features

- **Aggregation**: Statistical utilities for price aggregation, including median calculation and outlier filtering.
- **Price Providers**: Traits and generic implementations for efficient price data fetching and handling.
- **Types**: Shared data structures and configuration types used across the Charli3 ecosystem.
- **Encoding**: Utilities for CBOR encoding and hashing tailored for Cardano interoperability.

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
charli3-oracle-core = { git = "https://github.com/Charli3-Official/charli3-substrate-oracle-core", default-features = false }
```

## Configuration

The crate supports the following features:

- `std`: Enables standard library support. Enabled by default.
- `pallet`: Enables Substrate `frame-support` dependencies. Use this when integrating into a Substrate pallet.
