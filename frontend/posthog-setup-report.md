<wizard-report>
# PostHog post-wizard report

The wizard has completed a deep integration of PostHog analytics into the Pismo Protocol frontend — a React/Vite DeFi options trading app on Sui blockchain. A `posthog-js` singleton was initialized in `src/lib/posthog.ts` and imported at app boot via `main.tsx`. Users are identified by their Sui wallet address on connect, identities are reset on disconnect, and 10 events covering the full product lifecycle are instrumented across 6 files. Exception autocapture is enabled, and `captureException` calls are placed in all critical transaction error paths.

| Event | Description | File |
|---|---|---|
| `wallet_connected` | User connects their Sui wallet (top of acquisition funnel) | `src/components/Header.tsx` |
| `wallet_disconnected` | User disconnects their wallet | `src/components/Header.tsx` |
| `option_written` | User successfully writes (sells) a covered call via RFQ on the Earn screen | `src/state/composer.ts` |
| `option_purchased` | User successfully buys a call option via RFQ on the Buy screen | `src/state/composer.ts` |
| `option_exercised` | User exercises an in-the-money call option from the Dashboard | `src/state/dashboard.ts` |
| `position_claimed` | Writer claims settlement after their option series expires | `src/state/dashboard.ts` |
| `deepbook_trading_enabled` | User completes one-time DeepBook BalanceManager setup | `src/components/TradePanel.tsx` |
| `deepbook_order_placed` | User places a market or limit order on the DeepBook secondary market | `src/components/TradePanel.tsx` |
| `deepbook_funds_withdrawn` | User withdraws all funds from their DeepBook BalanceManager | `src/components/TradePanel.tsx` |
| `faucet_tokens_minted` | User mints test tokens on testnet via the Faucet screen | `src/screens/Faucet.tsx` |

## Next steps

We've built some insights and a dashboard for you to keep an eye on user behavior, based on the events we just instrumented:

- [Analytics basics (wizard) — Dashboard](https://us.posthog.com/project/466886/dashboard/1703051)
- [Trader conversion funnel (wizard)](https://us.posthog.com/project/466886/insights/YkSKCXDf) — wallet_connected → option_purchased conversion rate
- [Writer conversion funnel (wizard)](https://us.posthog.com/project/466886/insights/5bzvuXzc) — wallet_connected → option_written conversion rate
- [Options trading volume (wizard)](https://us.posthog.com/project/466886/insights/QiaXTfmY) — daily options written + purchased
- [Settlement activity (wizard)](https://us.posthog.com/project/466886/insights/RbQhV6A0) — daily options exercised + positions claimed
- [Unique active wallets (wizard)](https://us.posthog.com/project/466886/insights/OKuKotbn) — DAU by unique wallet address

### Agent skill

We've left an agent skill folder in your project. You can use this context for further agent development when using Claude Code. This will help ensure the model provides the most up-to-date approaches for integrating PostHog.

</wizard-report>
