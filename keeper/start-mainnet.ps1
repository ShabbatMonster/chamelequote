# Starts the keeper in the background on this machine (Windows). Stop it with:
#   Get-Process chamelequote-keeper | Stop-Process
# Logs go to ~\.config\chamelequote\keeper.log. The RPC URL comes from $env:RPC_URL, else from
# ~\.config\chamelequote\rpc.txt (kept out of the repo: it holds an API key), else the public node.

$dir = Join-Path $HOME ".config\chamelequote"
$rpcFile = Join-Path $dir "rpc.txt"
$rpc = if ($env:RPC_URL) { $env:RPC_URL }
       elseif (Test-Path $rpcFile) { (Get-Content $rpcFile -Raw).Trim() }
       else { "https://api.mainnet-beta.solana.com" }
$exe = Join-Path $PSScriptRoot "target\release\chamelequote-keeper.exe"

if (Get-Process chamelequote-keeper -ErrorAction SilentlyContinue) {
    Write-Output "keeper is already running"
    exit 0
}
Start-Process -FilePath $exe -WindowStyle Hidden `
    -ArgumentList @("run", "--rpc", $rpc, "--keypair", (Join-Path $dir "keeper.json"), "--priority-fee", "10000") `
    -RedirectStandardOutput (Join-Path $dir "keeper.log") `
    -RedirectStandardError (Join-Path $dir "keeper.err.log")
Write-Output "keeper started; log: $(Join-Path $dir 'keeper.log')"
