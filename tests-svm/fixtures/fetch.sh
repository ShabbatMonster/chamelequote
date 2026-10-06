#!/usr/bin/env bash
# Dumps the mainnet programs the LiteSVM tests load. Needs the Solana CLI.
set -euo pipefail
cd "$(dirname "$0")"
URL=${RPC_URL:-https://api.mainnet-beta.solana.com}
solana program dump -u "$URL" whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc whirlpool.so
solana program dump -u "$URL" metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s mpl_token_metadata.so
solana program dump -u "$URL" CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK raydium_clmm.so
# The program as deployed before the move to Raydium (for the migration test).
solana program dump -u "$URL" 3ZYVePG4LhBWH9JvhGcExo1ysX6mWAwTGavBvyMgM3Ws chamelequote_mainnet_v1.so
