#!/usr/bin/env bash
# Dumps the mainnet programs the LiteSVM tests load. Needs the Solana CLI.
set -euo pipefail
cd "$(dirname "$0")"
URL=${RPC_URL:-https://api.mainnet-beta.solana.com}
solana program dump -u "$URL" whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc whirlpool.so
solana program dump -u "$URL" metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s mpl_token_metadata.so
