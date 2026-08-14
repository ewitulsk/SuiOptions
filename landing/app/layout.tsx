import type { Metadata } from "next";
import "./globals.css";

export const metadata: Metadata = {
  title: "Pismo Protocol — Fully collateralized options on Sui",
  description:
    "Fully collateralized options, a hybrid exchange, and curated liquidity vaults on Sui — three products, one liquidity flywheel.",
};

export default function RootLayout({ children }: { children: React.ReactNode }) {
  return (
    <html lang="en">
      <head>
        <link rel="preconnect" href="https://fonts.googleapis.com" />
        <link rel="preconnect" href="https://fonts.gstatic.com" crossOrigin="anonymous" />
        <link
          href="https://fonts.googleapis.com/css2?family=Inter:wght@400;500;600;700;800&family=IBM+Plex+Mono:wght@400;500;600;700;800&display=swap"
          rel="stylesheet"
        />
        <link rel="icon" href="/pismo-mark.svg" type="image/svg+xml" />
      </head>
      <body>{children}</body>
    </html>
  );
}
