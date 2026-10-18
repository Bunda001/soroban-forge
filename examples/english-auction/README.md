# english-auction

English-auction contract template for iterative bids, minimum increments, and seller settlement flow.

Bids must exceed the current highest bid by a minimum increment configured in basis points (1 bps = 0.01%). A bid that lands within a configurable window of the end time extends the auction by the configured extension, preventing last-ledger sniping.

## Security

This template is a starting point, not audited code. See [security-considerations.md](https://github.com/soroban-forge-labs/soroban-forge/blob/main/docs/security-considerations.md) for what it does and does not protect against.
