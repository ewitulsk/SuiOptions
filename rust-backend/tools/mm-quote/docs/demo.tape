# VHS tape — records a quick tour of the `mm-quote` simulator.
# Run with `vhs tools/mm-quote/docs/demo.tape` from the rust-backend root.

Output tools/mm-quote/docs/demo.gif
Output tools/mm-quote/docs/demo.mp4

Set FontSize 14
Set Width 1200
Set Height 720
Set Theme "Dracula"
Set TypingSpeed 35ms
Set PlaybackSpeed 1.0
Set Padding 20

# Prebuild so the recording isn't dominated by `cargo` chatter.
Hide
Type "cd /Users/evanw./Dev/options/rust-backend && cargo build -q -p mm-quote && clear"
Enter
Sleep 8s
Show

# --- intro ---
Type "# mm-quote: simulate the mm-bot's premium for an RFQ given asset prices"
Sleep 800ms
Enter
Sleep 600ms

# --- 1) ATM 30-day BTC call ---
Type "# 1) 30-day ATM BTC call — $60k spot, σ=60%, write 1 BTC (1e8 raw)"
Enter
Sleep 400ms
Type "cargo run -q -p mm-quote -- \"
Enter
Type "  --underlying-usd 60000 --settlement-usd 1.0 \"
Enter
Type "  --underlying-decimals 8 --settlement-decimals 6 \"
Enter
Type "  --strike 600 --strike-scale 0 \"
Enter
Type "  --days-to-expiry 30 --write-amount 100000000 \"
Enter
Type "  --sigma 0.6 --rate 0.05"
Enter
Sleep 3s

# --- 2) far-OTM declines ---
Type "# 2) Far OTM, 7 days — bot declines (priced to zero)"
Enter
Sleep 400ms
Type "cargo run -q -p mm-quote -- --underlying-usd 60000 --underlying-decimals 8 --settlement-decimals 6 \"
Enter
Type "  --strike 2000 --days-to-expiry 7 --write-amount 100000000 --sigma 0.6"
Enter
Sleep 3s

# --- 3) expired-bucket intrinsic ---
Type "# 3) Expired bucket, ITM — quotes intrinsic, not declined"
Enter
Sleep 400ms
Type "cargo run -q -p mm-quote -- --underlying-usd 60000 --underlying-decimals 8 --settlement-decimals 6 \"
Enter
Type "  --strike 500 --days-to-expiry 0 --write-amount 100000000 --sigma 0.6"
Enter
Sleep 3s

# --- 4) JSON mode ---
Type "# 4) Machine-readable output for pipelines"
Enter
Sleep 400ms
Type "cargo run -q -p mm-quote -- --underlying-usd 60000 --underlying-decimals 8 --settlement-decimals 6 \"
Enter
Type "  --strike 700 --days-to-expiry 30 --write-amount 100000000 --sigma 0.6 --rate 0.05 --json"
Enter
Sleep 3s

Sleep 1s
