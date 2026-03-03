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

## License Terms

Terms

The Licensed Work is provided under the Business Source License 1.1.
On the Change Date, the Licensed Work will automatically be licensed
under the MIT License.

This license does not grant rights to use Charli3 Oracles trademarks,
logos, or branding.


What this allows:

- ✅ Full source visibility and auditability
- ✅ Internal and production use with the Charli3 hosted partner-chain
- ✅ Development of oracle templates, bridges, and data adapters
- ✅ Commercial sale of templates and adapters on the Charli3 marketplace
- ✅ Limited modification of the core to support extensions

What this restricts:

- ❌ Self-hosting or operating a competing partner-chain or oracle network
- ❌ Offering oracle-network-as-a-service using this software without a
  commercial license

Each release automatically becomes MIT licensed after 18 months.

For commercial licensing inquiries:
📧 sales@charli3.io
