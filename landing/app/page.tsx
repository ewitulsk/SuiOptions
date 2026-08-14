import RepelGrid from "@/components/superrr/RepelGrid";
import TypewriterCaret from "@/components/superrr/TypewriterCaret";
import LineByLineReveal from "@/components/superrr/LineByLineReveal";

const palette = {
  background: "#FCFDFF",
  surface: "#FFFFFF",
  sectionAltBackground: "#F6F9FF",
  textPrimary: "#222946",
  textSecondary: "#696D79",
  brand: "#0073D3",
  cta: "#0073D3",
  ctaForeground: "#FFFFFF",
  border: "#D2D4DD",
  extras: [
    { role: "terminal-bar-navy", value: "#222A4C" },
    { role: "pixel-numeral-blue", value: "#468ADC" },
    { role: "hatch-gutter-gray", value: "#E6E6E6" },
    { role: "status-dot-yellow", value: "#E8B93C" },
  ],
};

/* Pismo "ebb & flow" mark, recolored to the page's two blue tokens (no gradients in this direction) */
function PismoMark({ size = 28 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="10 10 44 44" fill="none" aria-label="Pismo Protocol" role="img">
      <defs>
        <path id="pm-swoosh" d="M12 30 C 26 30 31 20 50 20 C 39 24 35 34 16 35 Z" />
      </defs>
      <g transform="translate(-2,0)">
        <use href="#pm-swoosh" fill="#0073D3" />
        <use href="#pm-swoosh" fill="#468ADC" transform="rotate(180 32 32)" />
      </g>
    </svg>
  );
}

/* dithered-pixel-stat-numeral: one monumental figure set as a 1-bit checker field */
function DitherNumeral({
  id,
  figure,
  caption,
  sizePx = 128,
  width = 460,
}: {
  id: string;
  figure: string;
  caption: string;
  sizePx?: number;
  width?: number;
}) {
  const height = Math.round(sizePx * 1.38);
  return (
    <svg
      viewBox={`0 0 ${width} ${height}`}
      width={width}
      height={height}
      role="img"
      aria-label={`${figure} — ${caption}`}
    >
      <defs>
        <pattern id={`${id}-d`} width="8" height="8" patternUnits="userSpaceOnUse">
          <rect width="4" height="4" fill="#468ADC" />
          <rect x="4" y="4" width="4" height="4" fill="#468ADC" />
        </pattern>
        <pattern id={`${id}-s`} width="16" height="8" patternUnits="userSpaceOnUse">
          <rect width="4" height="4" fill="#468ADC" />
        </pattern>
      </defs>
      <text
        x="16"
        y={sizePx + 8}
        fontFamily="'IBM Plex Mono', monospace"
        fontWeight="600"
        fontSize={sizePx}
        letterSpacing="-1"
        fill={`url(#${id}-s)`}
      >
        {figure}
      </text>
      <text
        x="8"
        y={sizePx}
        fontFamily="'IBM Plex Mono', monospace"
        fontWeight="600"
        fontSize={sizePx}
        letterSpacing="-1"
        fill={`url(#${id}-d)`}
      >
        {figure}
      </text>
      <text
        x="8"
        y={height - 8}
        fontFamily="'IBM Plex Mono', monospace"
        fontSize="12"
        letterSpacing="1"
        fill="#696D79"
      >
        {caption}
      </text>
    </svg>
  );
}

/* Phosphor thin icons, inlined (currentColor) */
const ICONS = {
  coin: (
    <svg viewBox="0 0 256 256" fill="currentColor" aria-hidden="true">
      <path d="M205.79,67.42C185.9,57.48,158.27,52,128,52S70.1,57.48,50.21,67.42C31,77,20,90.35,20,104v48c0,13.65,11,27,30.21,36.58C70.1,198.52,97.73,204,128,204s57.9-5.48,77.79-15.42C225,179,236,165.65,236,152V104C236,90.35,225,77,205.79,67.42ZM128,60c61.77,0,100,22.84,100,44s-38.23,44-100,44S28,125.16,28,104,66.23,60,128,60ZM124,156v40c-22-.35-40.94-3.65-56-8.71V147.65C84.23,152.75,103.44,155.62,124,156Zm8,0c20.56-.33,39.77-3.2,56-8.3v39.59c-15.06,5.06-33.95,8.36-56,8.71ZM28,152V123.92c5.15,6.19,12.67,11.89,22.21,16.66,3.08,1.54,6.36,2.95,9.79,4.28v39.38C39.49,175.67,28,163.59,28,152Zm200,0c0,11.59-11.49,23.67-32,32.24V144.86c3.43-1.33,6.71-2.74,9.79-4.28,9.54-4.77,17.06-10.47,22.21-16.66Z" />
    </svg>
  ),
  swap: (
    <svg viewBox="0 0 256 256" fill="currentColor" aria-hidden="true">
      <path d="M220,48V152a12,12,0,0,1-12,12H89.66l17.17,17.17a4,4,0,0,1-5.66,5.66l-24-24a4,4,0,0,1,0-5.66l24-24a4,4,0,0,1,5.66,5.66L89.66,156H208a4,4,0,0,0,4-4V48a4,4,0,0,0-4-4H96a4,4,0,0,0-4,4v8a4,4,0,0,1-8,0V48A12,12,0,0,1,96,36H208A12,12,0,0,1,220,48ZM168,196a4,4,0,0,0-4,4v8a4,4,0,0,1-4,4H48a4,4,0,0,1-4-4V104a4,4,0,0,1,4-4H166.34l-17.17,17.17a4,4,0,0,0,5.66,5.66l24-24a4,4,0,0,0,0-5.66l-24-24a4,4,0,0,0-5.66,5.66L166.34,92H48a12,12,0,0,0-12,12V208a12,12,0,0,0,12,12H160a12,12,0,0,0,12-12v-8A4,4,0,0,0,168,196Z" />
    </svg>
  ),
  vault: (
    <svg viewBox="0 0 256 256" fill="currentColor" aria-hidden="true">
      <path d="M216,44H40A12,12,0,0,0,28,56V192a12,12,0,0,0,12,12H60v20a4,4,0,0,0,8,0V204H188v20a4,4,0,0,0,8,0V204h20a12,12,0,0,0,12-12V56A12,12,0,0,0,216,44Zm0,152H40a4,4,0,0,1-4-4V56a4,4,0,0,1,4-4H216a4,4,0,0,1,4,4v68H195.81a44,44,0,1,0,0,8H220v60A4,4,0,0,1,216,196Zm-52.7-72a12,12,0,1,0,0,8h24.47a36,36,0,1,1,0-8Z" />
    </svg>
  ),
};

export default function Page() {
  return (
    <div className="page">
      {/* ============ NAV ============ */}
      <header className="nav-wrap" data-section="nav" data-motion="idle ambient entrance">
        <div className="nav-strip">
          <div className="container">
            <span className="nav-strip__label">[ TESTNET LIVE ON SUI ]</span>
            <div className="nav-strip__type">
              <TypewriterCaret
                palette={palette}
                seed={11}
                speed={0.45}
                intensity={0.7}
                lead=""
                text={[
                  "quote, sign, settle. gas only on fills.",
                  "every option is an ordinary sui coin.",
                  "the curator trades. the curator never withdraws.",
                  "0 gas per requote, on both venues.",
                ]}
                style={{
                  background: "transparent",
                  justifyContent: "flex-start",
                  padding: 0,
                  ["--tw-fs" as string]: "12px",
                }}
              />
            </div>
          </div>
        </div>
        <div className="nav-bar">
          <div className="container">
            <a className="nav-bar__brand" href="#top" aria-label="Pismo Protocol home">
              <PismoMark />
              <span className="nav-bar__wordmark">PISMO</span>
            </a>
            <nav className="nav-bar__links" aria-label="Primary">
              <a href="#options"><b>01</b>OPTIONS</a>
              <a href="#exchange"><b>02</b>EXCHANGE</a>
              <a href="#vaults"><b>03</b>VAULTS</a>
              <a href="https://docs.sui-options.com"><b>04</b>DOCS</a>
              <a className="fill-btn nav-bar__cta" href="https://sui-options.com">LAUNCH APP</a>
            </nav>
          </div>
        </div>
      </header>

      {/* ============ HERO ============ */}
      <section className="hero" id="top" data-section="hero" data-motion="pointer entrance">
        <RepelGrid palette={palette} density={0.18} speed={0.5} seed={4} intensity={0.3} reach={0.45} />
        <div className="container hero__grid">
          <div className="hero__copy" data-reveal>
            <p className="hero__eyebrow">
              PISMO PROTOCOL <b>·</b> OPTIONS / EXCHANGE / VAULTS ON SUI
            </p>
            <h1>
              Fully collateralized options, written as ordinary coins.
            </h1>
            <p className="hero__sub">
              American-style calls and puts on Sui, priced by competing market makers and settled
              atomically on-chain — backed 1:1 from the moment they exist.
            </p>
            <div className="hero__ctas">
              <a className="fill-btn" href="https://sui-options.com">LAUNCH TESTNET APP</a>
              <a className="tick-btn" href="https://docs.sui-options.com">READ THE DOCS</a>
            </div>
            <p className="hero__note">
              SPLIT IT · TRANSFER IT · SELL IT ON PISMO EXCHANGE — IT&apos;S A COIN
            </p>
          </div>
          <div data-reveal>
            <figure className="crop">
              <span className="crop__mark crop__mark--tl" />
              <span className="crop__mark crop__mark--tr" />
              <span className="crop__mark crop__mark--bl" />
              <span className="crop__mark crop__mark--br" />
              <div className="rfq">
                <p className="rfq__tag">[ FIG. 01 / QUOTE REQUEST ]</p>
                <div className="rfq__body">
                  <ul className="rfq__rail" aria-hidden="true">
                    <li className="is-active">TRADE</li>
                    <li>EXCHANGE</li>
                    <li>VAULTS</li>
                    <li>PORTFOLIO</li>
                  </ul>
                  <div className="rfq__main">
                    <p className="rfq__req">
                      <b>BUY 25 × SUI CALL 3.20</b>
                      <span>EXP 2026-08-29</span>
                    </p>
                    <div className="rfq__quotes">
                      <p className="rfq__quotes-head">
                        <span>MAKER</span>
                        <span>PREMIUM / CONTRACT · TUSDC</span>
                      </p>
                      <p className="rfq__quote rfq__quote--best">
                        <em>MM-01</em>
                        <i />
                        <b>0.1420</b>
                        <span className="rfq__flag">BEST</span>
                      </p>
                      <p className="rfq__quote">
                        <em>MM-04</em>
                        <i />
                        <b>0.1435</b>
                      </p>
                      <p className="rfq__quote rfq__quote--wait">
                        <em>MM-02</em>
                        <i />
                        <b><span className="rfq__dot" />QUOTING…</b>
                      </p>
                    </div>
                    <p className="rfq__foot">
                      SIGNED QUOTE EMBEDS IN YOUR TX · RE-VERIFIED ON-CHAIN AT FILL
                    </p>
                  </div>
                </div>
              </div>
            </figure>
          </div>
        </div>
      </section>

      {/* ============ PROBLEM STATEMENT ============ */}
      <section className="problem" data-section="problem-statement" data-motion="scroll entrance">
        <div className="container problem__grid">
          <div data-reveal>
            <p className="eyebrow">[ 001 / STATUS QUO ]</p>
            <h2>On-chain options markets punish the makers who quote them.</h2>
          </div>
          <div>
            <div className="problem__reveal">
              <LineByLineReveal
                palette={palette}
                seed={9}
                speed={0.55}
                density={0.6}
                intensity={0.8}
                as="p"
                eyebrow=""
                footer="Stale quotes are the largest hidden cost in on-chain market making."
                text={[
                  "Every price update is a paid tx,",
                  "so books go stale between blocks.",
                  "AMM curves quote wide, and their",
                  "vol lags the market it prices.",
                  "Margin engines add liquidation",
                  "risk nobody asked to carry.",
                ]}
              />
            </div>
            <div className="problem__cells" data-reveal>
              <div className="problem__cell">
                <p className="problem__cell-index">[1]</p>
                <p className="problem__cell-figure">1 TX</p>
                <p className="problem__cell-caption">
                  per price update on a fully on-chain order book — repricing costs gas every time
                  the market moves.
                </p>
              </div>
              <div className="problem__cell">
                <p className="problem__cell-index">[2]</p>
                <div className="problem__cell-figure">
                  <DitherNumeral
                    id="pz"
                    figure="0"
                    caption="VENUES, BEFORE PISMO EXCHANGE"
                    sizePx={88}
                    width={300}
                  />
                </div>
                <p className="problem__cell-caption">
                  venues on Sui offered free quoting while staying composable with swap routers.
                </p>
              </div>
              <div className="problem__cell">
                <p className="problem__cell-index">[3]</p>
                <p className="problem__cell-figure">100%</p>
                <p className="problem__cell-caption">
                  of a written call&apos;s collateral stays locked until expiry under naive 1:1
                  backing.
                </p>
              </div>
            </div>
            <p className="hero__note" data-reveal>
              PISMO INVERTS THE COST MODEL: QUOTING IS FREE EVERYWHERE, AND A FILL EITHER VERIFIES
              THE MAKER&apos;S SIGNED PRICE ON-CHAIN OR NOTHING HAPPENS AT ALL.
            </p>
          </div>
        </div>
      </section>

      {/* ============ LOGO CLOUD / VENUES ============ */}
      <section className="venues" data-section="logo-cloud" data-motion="scroll entrance">
        <div className="container" data-reveal>
          <p className="venues__label">Venues &amp; infrastructure the protocol plugs into</p>
          <div className="venues__row">
            <span className="venues__cell">SUI</span>
            <span className="venues__cell">DEEPBOOK</span>
            <span className="venues__cell">BLUEFIN</span>
            <span className="venues__cell">CIRCLE CCTP</span>
            <span className="venues__cell">SWITCHBOARD</span>
            <span className="venues__cell">AFTERMATH&nbsp;<small>NEXT</small></span>
          </div>
          <div className="venues__ruler" aria-hidden="true">
            <span className="venues__cursor" />
          </div>
        </div>
      </section>

      {/* ============ HOW IT WORKS (inverted) ============ */}
      <section className="how" id="options" data-section="how-it-works" data-motion="state entrance">
        <div className="container">
          <div className="how__plate" data-reveal>
            <div className="how__head">
              <p className="how__eyebrow">[ 002 / EXECUTION PATH ]</p>
              <h2>Quote, sign, settle — one atomic transaction.</h2>
              <p className="how__sub">
                No pool, no pricing curve. Your request is answered by real market makers, and the
                chain re-verifies the exact terms they signed before anything moves.
              </p>
            </div>
            <div className="how__cols">
              <ol className="how__steps">
                <li>
                  <p className="how__step-index">01 / REQUEST</p>
                  <p className="how__step-title">Broadcast to every maker</p>
                  <p className="how__step-copy">
                    Your order goes out over WebSocket to every connected market maker at once.
                  </p>
                </li>
                <li>
                  <p className="how__step-index">02 / PICK A QUOTE</p>
                  <p className="how__step-title">Signed, executable prices</p>
                  <p className="how__step-copy">
                    Quotes arrive in seconds. Each one is signed over the exact economic terms —
                    asset, amount, price, expiry, funding source.
                  </p>
                </li>
                <li>
                  <p className="how__step-index">03 / SETTLE ON-CHAIN</p>
                  <p className="how__step-title">The price you saw, or nothing</p>
                  <p className="how__step-copy">
                    The signed quote embeds in your transaction. The chain re-verifies it and moves
                    collateral atomically — or the whole thing reverts.
                  </p>
                </li>
              </ol>

              {/* stage-plate-register: one real run, chained figures */}
              <div className="stage" data-stage="request">
                <div className="stage__bar">
                  <span>RUN / BUY 25 × SUI CALL 3.20 · EXP 2026-08-29</span>
                  <div
                    className="stage__pins"
                    role="radiogroup"
                    aria-label="Step through the stages of one fill"
                    data-operable="stage-plate-register"
                  >
                    <button className="stage__pin" type="button" role="radio" data-value="request" aria-checked="true" tabIndex={0}>REQUEST</button>
                    <button className="stage__pin" type="button" role="radio" data-value="quotes" aria-checked="false" tabIndex={-1}>QUOTES</button>
                    <button className="stage__pin" type="button" role="radio" data-value="accept" aria-checked="false" tabIndex={-1}>ACCEPT</button>
                    <button className="stage__pin" type="button" role="radio" data-value="settle" aria-checked="false" tabIndex={-1}>SETTLE</button>
                  </div>
                </div>
                <p className="stage__fig">[ STAGE 1 / REQUEST ]</p>
                <p className="stage__desc">
                  The order broadcasts to every connected maker. No on-chain state is touched yet.
                </p>
                <dl className="stage__cells">
                  <div className="stage__cell"><dt>MAKERS PINGED</dt><dd>6</dd></div>
                  <div className="stage__cell"><dt>QUOTES IN</dt><dd>0</dd></div>
                  <div className="stage__cell"><dt>TX SENT</dt><dd>0</dd></div>
                </dl>
                <div className="stage__progress"><i style={{ width: "4%" }} /></div>
                <p className="stage__elapsed">
                  <span className="stage__elapsed-line">ELAPSED 180 OF 4,100 MS</span>
                  <span className="stage__pct">4% OF RUN</span>
                </p>
              </div>
            </div>
          </div>
        </div>
      </section>

      {/* ============ FEATURES GRID (full-bleed) ============ */}
      <section className="products" id="exchange" data-section="features-grid" data-motion="state entrance">
        <div className="container">
          <div className="products__head" data-reveal>
            <p className="eyebrow">[ 003 / PRODUCT SUITE ]</p>
            <h2>Three products, one pool of capital.</h2>
          </div>
          <div className="products__grid" data-reveal>
            <div className="products__col">
              <p className="products__index">01 / PISMO OPTIONS</p>
              <div className="products__icon">{ICONS.coin}</div>
              <p className="products__name">Options that live in your wallet</p>
              <p className="products__copy">
                American-style calls and puts, minted 1:1 against locked collateral. Every series is
                a real fungible Sui coin — exercise any amount, any time before expiry.
              </p>
            </div>
            <div className="products__col">
              <p className="products__index">02 / PISMO EXCHANGE</p>
              <div className="products__icon">{ICONS.swap}</div>
              <p className="products__name">Free quoting, on-chain fills</p>
              <p className="products__copy">
                Order books live off-chain, so placing, repricing and cancelling cost nothing. Every
                fill settles on-chain and composes with swap routers like any AMM.
              </p>
            </div>
            <div className="products__col" id="vaults">
              <p className="products__index">03 / PISMO VAULTS</p>
              <div className="products__icon">{ICONS.vault}</div>
              <p className="products__name">The curator trades, never takes</p>
              <p className="products__copy">
                Depositors pool capital; a curator market-makes with it across venues. The Move type
                system — not a promise — stops them withdrawing it.
              </p>
            </div>
          </div>

          <div className="flywheel-wrap" data-reveal>
            <figure className="flywheel">
              <figcaption className="flywheel__tag">[ FIG. 02 / THE LIQUIDITY FLYWHEEL ]</figcaption>
              {/* desktop flywheel schematic */}
              <svg className="flywheel--d" viewBox="0 0 640 320" fontFamily="'IBM Plex Mono', monospace" role="img" aria-label="Flywheel: vaults supply capital to options and exchange; inventory and proceeds flow back">
                <defs>
                  <pattern id="fw-gp" width="16" height="16" patternUnits="userSpaceOnUse">
                    <path d="M16 0H0V16" fill="none" stroke="#D2D4DD" strokeWidth="0.5" />
                  </pattern>
                </defs>
                <rect width="640" height="320" fill="#FFFFFF" />
                <rect width="640" height="320" fill="url(#fw-gp)" />
                <path d="M8 24V8h16M616 8h16v16M8 296v16h16M632 312v-16h-16" fill="none" stroke="#222946" />
                {/* vaults node */}
                <rect x="48" y="120" width="152" height="72" fill="#222A4C" />
                <text x="66" y="152" fontSize="12" letterSpacing="1" fill="#FFFFFF">VAULTS</text>
                <text x="66" y="172" fontSize="9" letterSpacing="1" fill="#FFFFFF" opacity="0.65">POOLED CAPITAL</text>
                {/* options node */}
                <rect x="400" y="40" width="184" height="64" fill="#FCFDFF" stroke="#222946" />
                <text x="418" y="68" fontSize="12" letterSpacing="1" fill="#222946">OPTIONS</text>
                <text x="418" y="86" fontSize="9" letterSpacing="1" fill="#696D79">SIGNED RFQ QUOTES</text>
                <circle cx="560" cy="62" r="3" fill="#E8B93C" />
                <text x="536" y="94" fontSize="8" letterSpacing="1" fill="#696D79" />
                {/* exchange node */}
                <rect x="400" y="208" width="184" height="64" fill="#FCFDFF" stroke="#222946" />
                <text x="418" y="236" fontSize="12" letterSpacing="1" fill="#222946">EXCHANGE</text>
                <text x="418" y="254" fontSize="9" letterSpacing="1" fill="#696D79">MAKER-SIGNED BOOKS</text>
                {/* routes */}
                <path d="M200 140 H320 V72 H396" fill="none" stroke="#0073D3" strokeDasharray="4 4" />
                <text x="228" y="132" fontSize="9" letterSpacing="1" fill="#0073D3">CAPITAL BACKS QUOTES</text>
                <path d="M200 172 H320 V240 H396" fill="none" stroke="#0073D3" strokeDasharray="4 4" />
                <text x="228" y="262" fontSize="9" letterSpacing="1" fill="#0073D3">DIRECT ESCROW</text>
                <path d="M492 104 V204" fill="none" stroke="#696D79" strokeDasharray="4 4" />
                <text x="502" y="158" fontSize="9" letterSpacing="1" fill="#696D79">INVENTORY OFFLOADS</text>
                <path d="M400 250 H124 V196" fill="none" stroke="#696D79" strokeDasharray="4 4" />
                <text x="152" y="288" fontSize="9" letterSpacing="1" fill="#696D79">PROCEEDS RETURN, SAME TX</text>
                {/* external venues chip */}
                <rect x="48" y="40" width="152" height="32" fill="#FFFFFF" stroke="#D2D4DD" />
                <text x="60" y="60" fontSize="9" letterSpacing="1" fill="#696D79">DEEPBOOK · BLUEFIN</text>
                <path d="M124 72 V116" fill="none" stroke="#D2D4DD" strokeDasharray="4 4" />
              </svg>
              {/* mobile flywheel: vertical rail */}
              <svg className="flywheel--m" viewBox="0 0 358 380" fontFamily="'IBM Plex Mono', monospace" role="img" aria-label="Flywheel: vaults supply capital to options and exchange; inventory and proceeds flow back">
                <rect width="358" height="380" fill="#FFFFFF" />
                <path d="M8 24V8h16M334 8h16v16" fill="none" stroke="#222946" />
                <rect x="16" y="32" width="326" height="64" fill="#222A4C" />
                <text x="32" y="60" fontSize="12" letterSpacing="1" fill="#FFFFFF">VAULTS</text>
                <text x="32" y="80" fontSize="9" letterSpacing="1" fill="#FFFFFF" opacity="0.65">POOLED CAPITAL</text>
                <path d="M40 96 V140" fill="none" stroke="#0073D3" strokeDasharray="4 4" />
                <text x="52" y="124" fontSize="9" letterSpacing="1" fill="#0073D3">CAPITAL BACKS QUOTES</text>
                <rect x="16" y="140" width="326" height="64" fill="#FCFDFF" stroke="#222946" />
                <text x="32" y="168" fontSize="12" letterSpacing="1" fill="#222946">OPTIONS</text>
                <text x="32" y="188" fontSize="9" letterSpacing="1" fill="#696D79">SIGNED RFQ QUOTES</text>
                <circle cx="318" cy="162" r="3" fill="#E8B93C" />
                <path d="M40 204 V248" fill="none" stroke="#696D79" strokeDasharray="4 4" />
                <text x="52" y="232" fontSize="9" letterSpacing="1" fill="#696D79">INVENTORY OFFLOADS</text>
                <rect x="16" y="248" width="326" height="64" fill="#FCFDFF" stroke="#222946" />
                <text x="32" y="276" fontSize="12" letterSpacing="1" fill="#222946">EXCHANGE</text>
                <text x="32" y="296" fontSize="9" letterSpacing="1" fill="#696D79">MAKER-SIGNED BOOKS</text>
                <path d="M40 312 V356" fill="none" stroke="#696D79" strokeDasharray="4 4" />
                <text x="52" y="340" fontSize="9" letterSpacing="1" fill="#696D79">PROCEEDS RETURN, SAME TX</text>
              </svg>
            </figure>

            {/* queue-status-filter, repointed at the flywheel ledger */}
            <div className="ledger" data-filter="all">
              <div className="ledger__bar">
                <span>FLOW LEDGER &gt;&gt;&gt;</span>
                <div
                  className="ledger__chips"
                  role="group"
                  aria-label="Filter the flow ledger by product"
                  data-operable="queue-status-filter"
                >
                  <button className="ledger__chip" type="button" data-value="all" aria-pressed="true">ALL <b>12</b></button>
                  <button className="ledger__chip" type="button" data-value="options" aria-pressed="false">OPTIONS <b>5</b></button>
                  <button className="ledger__chip" type="button" data-value="exchange" aria-pressed="false">EXCHANGE <b>4</b></button>
                  <button className="ledger__chip" type="button" data-value="vaults" aria-pressed="false">VAULTS <b>3</b></button>
                </div>
              </div>
              <ol className="ledger__log">
                {(
                  [
                    ["F-01", "collateral locks, option coins mint 1:1", "options"],
                    ["F-02", "premium pays the writer instantly", "options"],
                    ["F-03", "FIFO cursor assigns exercises — no lottery", "options"],
                    ["F-04", "offset closure frees collateral mid-cycle", "options"],
                    ["F-05", "spread compression escrows the long call", "options"],
                    ["F-06", "maker posts signed orders over HTTP, free", "exchange"],
                    ["F-07", "fill re-verifies the signature on-chain", "exchange"],
                    ["F-08", "router flow hits maker quotes atomically", "exchange"],
                    ["F-09", "owner withdrawal works even when paused", "exchange"],
                    ["F-10", "deposits mint shares, per-user cost basis", "vaults"],
                    ["F-11", "one pool quotes both venues at once", "vaults"],
                    ["F-12", "every venue path returns funds to the vault", "vaults"],
                  ] as [string, string, string][]
                ).map(([id, text, flow]) => (
                  <li className="ledger__row" data-flow={flow} key={id}>
                    <span>{id}</span>
                    <em>{text}</em>
                    <i />
                    <b>{flow.toUpperCase()}</b>
                  </li>
                ))}
              </ol>
              <p className="ledger__tally">SHOWING 12 OF 12 FLOWS · 100.0%</p>
            </div>
          </div>
        </div>
      </section>

      {/* ============ CENTERPIECE: CAPITAL EFFICIENCY (mono only, numbers carry it) ============ */}
      <section className="capital" data-section="feature-spotlight" data-motion="state entrance">
        <div className="container">
          <div className="capital__head" data-reveal>
            <div>
              <p className="capital__eyebrow">[ 004 / CAPITAL EFFICIENCY ]</p>
              <p className="capital__title">One balance sheet, zero dead capital.</p>
            </div>
            <p className="capital__note">
              NO LEVERAGE. NO UNDERCOLLATERALIZATION. EVERY SAVING COMES FROM DELETING COLLATERAL
              THAT SECURES NOTHING.
            </p>
          </div>
          <div className="capital__body" data-reveal>
            <div
              className="capital__tabs"
              role="tablist"
              aria-label="Capital efficiency, itemized"
              data-metric="requote"
              data-operable="benchmark-contour-tabs"
            >
              <button className="capital__tab" type="button" role="tab" id="cap-tab-requote" aria-controls="cap-panel-requote" data-value="requote" aria-selected="true" tabIndex={0}>
                REQUOTE COST <small>[ A ]</small>
              </button>
              <button className="capital__tab" type="button" role="tab" id="cap-tab-venues" aria-controls="cap-panel-venues" data-value="venues" aria-selected="false" tabIndex={-1}>
                VENUES ONE POOL BACKS <small>[ B ]</small>
              </button>
              <button className="capital__tab" type="button" role="tab" id="cap-tab-backing" aria-controls="cap-panel-backing" data-value="backing" aria-selected="false" tabIndex={-1}>
                BACKED, IN EVERY STATE <small>[ C ]</small>
              </button>
            </div>
            <div className="capital__stage">
              <div className="capital__panel" id="cap-panel-requote" role="tabpanel" aria-labelledby="cap-tab-requote">
                <div className="capital__numeral">
                  <DitherNumeral id="cnA" figure="0" caption="GAS PER PRICE UPDATE · OPTIONS + EXCHANGE" sizePx={150} width={640} />
                </div>
                <dl className="capital__ledger">
                  <div className="row"><span>A QUOTE IS</span><i /><b>SIGNED BYTES, NOT AN OBJECT</b></div>
                  <div className="row"><span>REPRICE</span><i /><b>FREE, 100×/MIN IF YOU LIKE</b></div>
                  <div className="row"><span>CANCEL</span><i /><b>NOTHING TO CANCEL — QUOTES EXPIRE</b></div>
                  <div className="row"><span>CHAIN TOUCHED</span><i /><b>ONLY WHEN A FILL EXECUTES</b></div>
                </dl>
              </div>
              <div className="capital__panel" id="cap-panel-venues" role="tabpanel" aria-labelledby="cap-tab-venues" hidden>
                <div className="capital__numeral">
                  <DitherNumeral id="cnB" figure="2" caption="VENUES BACKED BY THE SAME VAULT DOLLAR, AT ONCE" sizePx={150} width={640} />
                </div>
                <dl className="capital__ledger">
                  <div className="row"><span>OPTIONS QUOTES</span><i /><b>BACKED STRAIGHT BY VAULT BALANCES</b></div>
                  <div className="row"><span>EXCHANGE ORDERS</span><i /><b>DIRECT ESCROW — NO SEPARATE ACCOUNT</b></div>
                  <div className="row"><span>CAPITAL MOVED BETWEEN VENUES</span><i /><b>0</b></div>
                  <div className="row"><span>CAN&apos;T COVER A FILL</span><i /><b>TX REVERTS ATOMICALLY</b></div>
                </dl>
              </div>
              <div className="capital__panel" id="cap-panel-backing" role="tabpanel" aria-labelledby="cap-tab-backing" hidden>
                <div className="capital__numeral">
                  <DitherNumeral id="cnC" figure="100%" caption="OF EVERY OPTION BACKED, IN EVERY STATE" sizePx={150} width={640} />
                </div>
                <dl className="capital__ledger">
                  <div className="row"><span>WRITTEN CALL</span><i /><b>FULL UNDERLYING LOCKED AT MINT</b></div>
                  <div className="row"><span>OFFSET CLOSURE</span><i /><b>NETS WRITE VS BUY-BACK, FREES COLLATERAL</b></div>
                  <div className="row"><span>SPREAD COMPRESSION</span><i /><b>A LONG CALL COLLATERALIZES THE SHORT</b></div>
                  <div className="row"><span>LIQUIDATION ENGINE</span><i /><b>NONE — NOTHING TO LIQUIDATE</b></div>
                </dl>
              </div>
            </div>
          </div>
        </div>
        {/* contour-terrain-plot, living variant: the page's one section-scale living artefact */}
        <div className="capital__terrain" role="img" aria-label="Chart recorder: blueprint terrain of layered contour lines with survey nodes">
          <p className="capital__terrain-tag">[ CHART RECORDER / VAULT QUOTE FEED ]</p>
          <svg viewBox="0 0 640 180" preserveAspectRatio="xMidYMid slice" aria-hidden="true">
            <defs>
              <g id="terrain-sheet">
                <path d="M0 138 C80 120, 160 152, 240 136 C320 120, 400 154, 480 132 C560 116, 600 146, 640 138" fill="none" stroke="#D2D4DD" />
                <path d="M0 108 C90 88, 170 122, 250 102 C330 84, 410 120, 490 98 C560 82, 600 118, 640 108" fill="none" stroke="#468ADC" strokeOpacity="0.55" />
                <path d="M0 78 C84 62, 168 92, 252 72 C336 52, 420 86, 504 66 C568 52, 608 88, 640 78" fill="none" stroke="#468ADC" />
                <path d="M0 50 C96 36, 176 62, 256 44 C336 26, 428 58, 508 40 C572 28, 612 58, 640 50" fill="none" stroke="#0073D3" strokeOpacity="0.8" />
                <circle cx="252" cy="72" r="3" fill="#E8B93C" />
                <circle cx="410" cy="104" r="3" fill="#E8B93C" />
                <circle cx="508" cy="40" r="3" fill="#E8B93C" />
                <rect x="288" y="124" width="44" height="18" fill="#222A4C" />
                <text x="296" y="136" fontFamily="'IBM Plex Mono', monospace" fontSize="8" letterSpacing="1" fill="#FFFFFF">RUN</text>
              </g>
            </defs>
            <g className="terrain-feed" data-live="contour-terrain-plot">
              <use href="#terrain-sheet" />
              <use href="#terrain-sheet" x="640" />
            </g>
          </svg>
        </div>
      </section>

      {/* ============ TRUST (testimonials slot) ============ */}
      <section className="trust" id="trust" data-section="testimonials" data-motion="state entrance">
        <div className="container">
          <div className="trust__head" data-reveal>
            <p className="eyebrow">[ 005 / LIMITATIONS &amp; TRUST ]</p>
            <h2>Services are trusted for liveness, never for funds.</h2>
            <p className="trust__sub">
              Pismo runs off-chain infrastructure — quote routing, matching, maintenance cranks. If
              every server disappeared tomorrow, funds would remain recoverable through
              permissionless on-chain paths. Open each record; we&apos;d rather be honest than
              reassuring.
            </p>
          </div>
          <div className="trust__cards" data-reveal>
            {[
              {
                tag: "[ 01 / QUOTE ROUTER ]",
                claim:
                  "Routes your request to every maker and ranks their quotes. It can go down; it can't change a price.",
                rows: [
                  ["can", "Broadcast quote requests to connected makers"],
                  ["can", "Deprioritize makers whose quotes revert"],
                  ["cannot", "Alter the signed terms of any quote"],
                  ["cannot", "Touch collateral or premiums"],
                ],
              },
              {
                tag: "[ 02 / EXCHANGE MATCHER ]",
                claim:
                  "Trusted for matching liveness and fairness — nothing else. Every fill re-verifies the maker's signature on-chain.",
                rows: [
                  ["can", "Match orders on price-time priority"],
                  ["can", "Censor or stall — a liveness risk, stated plainly"],
                  ["cannot", "Forge or misprice a fill"],
                  ["cannot", "Block withdrawal — owner exit works even paused"],
                ],
              },
              {
                tag: "[ 03 / VAULT CURATOR ]",
                claim:
                  "Trades depositor capital across venues. The Move type system — not a promise — stops them taking it.",
                rows: [
                  ["can", "Deploy funds to allowlisted venue integrations"],
                  ["can", "Lose money through bad trading — no contract prevents that"],
                  ["cannot", "Withdraw vault funds to themselves"],
                  ["cannot", "Move value out except depositor withdrawals"],
                ],
              },
            ].map((card, ci) => (
              <article className="trust-card" data-record="sealed" key={card.tag}>
                <p className="trust-card__tag">{card.tag}</p>
                <p className="trust-card__claim">{card.claim}</p>
                <div className="trust-card__strip">
                  <span>[ RECORD ]</span>
                  <i />
                  <span className="trust-card__state">SEALED</span>
                </div>
                <button
                  className="trust-card__open"
                  type="button"
                  data-operable="deployment-record-disclosure"
                  aria-expanded="false"
                  aria-controls={`trust-rec-${ci}`}
                >
                  <span className="trust-card__mark" aria-hidden="true" />
                  OPEN THE RECORD
                </button>
                <dl className="trust-card__rec" id={`trust-rec-${ci}`} hidden>
                  {card.rows.map(([kind, text]) => (
                    <div className="trust-card__row" data-kind={kind} key={text}>
                      <dt>{kind === "can" ? "CAN" : "CANNOT"}</dt>
                      <dd>{text}</dd>
                    </div>
                  ))}
                </dl>
              </article>
            ))}
          </div>
        </div>
      </section>

      {/* ============ CTA BANNER ============ */}
      <section className="cta" data-section="cta-banner" data-motion="pointer entrance">
        <RepelGrid palette={palette} density={0.16} speed={0.5} seed={23} intensity={0.28} reach={0.4} />
        <div className="container cta__grid">
          <div data-reveal>
            <p className="eyebrow">[ 006 / START ]</p>
            <h2>Write your first covered call on testnet.</h2>
            <p className="cta__sub">
              The app, the exchange and the flagship vault run on Sui testnet today — mainnet is in
              progress. Testnet SUI is free, and quoting costs nothing.
            </p>
            <div className="cta__ctas">
              <a className="fill-btn" href="https://sui-options.com">LAUNCH TESTNET APP</a>
              <a className="tick-btn" href="https://docs.sui-options.com">MARKET-MAKING &amp; VAULT DOCS</a>
            </div>
          </div>
          <div data-reveal>
            <div className="vaultmini">
              <p className="vaultmini__bar">
                <span>PISMO DESK VAULT · CURATED</span>
                <span>TESTNET</span>
              </p>
              <dl className="vaultmini__cells">
                <div className="vaultmini__cell"><dt>TVL</dt><dd>182,400 TUSDC</dd></div>
                <div className="vaultmini__cell"><dt>SHARE PRICE</dt><dd>1.0212</dd></div>
                <div className="vaultmini__cell"><dt>VENUES LIVE</dt><dd>4</dd></div>
              </dl>
              <p className="vaultmini__venues">
                <b>OPTIONS</b>
                <b>EXCHANGE</b>
                <b>DEEPBOOK</b>
                <b className="is-warn">BLUEFIN · BUDGET 62% USED</b>
              </p>
              <p className="vaultmini__foot">CURATOR: TRADES THE MONEY · CAN NEVER TAKE IT</p>
            </div>
          </div>
        </div>
      </section>

      {/* ============ FOOTER ============ */}
      <footer className="footer" data-section="footer" data-motion="ambient entrance">
        <div className="container">
          <div className="footer__grid">
            <div className="footer__brandcol">
              <p className="footer__brand">
                <PismoMark size={24} />
                <span className="nav-bar__wordmark">PISMO PROTOCOL</span>
              </p>
              <p className="footer__tagline">
                Three trading products on Sui — options, a hybrid exchange, and curated vaults — one
                liquidity flywheel.
              </p>
            </div>
            <div className="footer__col">
              <h3>Products</h3>
              <ul>
                <li><a href="#options">Pismo Options</a></li>
                <li><a href="#exchange">Pismo Exchange</a></li>
                <li><a href="#vaults">Pismo Vaults</a></li>
              </ul>
            </div>
            <div className="footer__col">
              <h3>Protocol</h3>
              <ul>
                <li><a href="https://docs.sui-options.com">Documentation</a></li>
                <li><a href="https://docs.sui-options.com">Capital Efficiency</a></li>
                <li><a href="#trust">Limitations &amp; Trust</a></li>
              </ul>
            </div>
            <div className="footer__col">
              <h3>Start</h3>
              <ul>
                <li><a href="https://sui-options.com">Launch app</a></li>
                <li><a href="https://docs.sui-options.com">Market making</a></li>
                <li><a href="https://docs.sui-options.com">For depositors</a></li>
              </ul>
            </div>
          </div>
          <div className="footer__status">
            <span>[ STATUS ]</span>
            <i className="footer__status-leader" />
            <span className="footer__status-state">
              <b className="footer__dot" data-live="operational-status-line" />
              TESTNET OPERATIONAL
            </span>
          </div>
          <p className="footer__legal">
            <span>© 2026 PISMO PROTOCOL</span>
            <span>RUNNING ON SUI TESTNET · MAINNET IN PROGRESS</span>
          </p>
        </div>
      </footer>

      {/* single closing full-bleed blue band */}
      <div className="closer">
        <div className="container">
          <span>PISMO PROTOCOL</span>
          <span>QUOTE FREE · SETTLE ON-CHAIN</span>
          <span>[ BUILT ON SUI ]</span>
        </div>
      </div>

      <script dangerouslySetInnerHTML={{ __html: PAGE_SCRIPT }} />
    </div>
  );
}

const PAGE_SCRIPT = `
(function () {
  "use strict";

  /* All init runs AFTER window load so React hydration sees the exact
     server-rendered DOM; every initial state below is already in the HTML. */
  function init() {

  /* ---------- entrance: fade-up-on-scroll, once, settled resting frame ---------- */
  var motionOK = !window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  if (motionOK && "IntersectionObserver" in window) {
    var els = Array.prototype.slice.call(document.querySelectorAll("[data-reveal]"));
    var vh = window.innerHeight;
    els.forEach(function (el) {
      var r = el.getBoundingClientRect();
      if (r.top > vh * 0.9) el.classList.add("reveal-pre");
    });
    var io = new IntersectionObserver(function (entries) {
      entries.forEach(function (e) {
        if (e.isIntersecting) {
          e.target.classList.add("reveal-in");
          e.target.classList.remove("reveal-pre");
          io.unobserve(e.target);
        }
      });
    }, { threshold: 0.15 });
    els.forEach(function (el) { io.observe(el); });
    /* safety net: nothing may rest hidden after a fast programmatic scroll */
    setInterval(function () {
      document.querySelectorAll(".reveal-pre").forEach(function (el) {
        var r = el.getBoundingClientRect();
        if (r.top < window.innerHeight && r.bottom > 0) {
          el.classList.add("reveal-in");
          el.classList.remove("reveal-pre");
        }
      });
    }, 400);
  }

  /* ---------- stage-plate-register: one run, chained figures ---------- */
  (function () {
    var STAGES = {
      request: {
        fig: "[ STAGE 1 / REQUEST ]",
        desc: "The order broadcasts to every connected maker. No on-chain state is touched yet.",
        cells: [["MAKERS PINGED", "6", ""], ["QUOTES IN", "0", ""], ["TX SENT", "0", ""]],
        elapsed: 180
      },
      quotes: {
        fig: "[ STAGE 2 / QUOTES ]",
        desc: "Signed, executable prices come back over the same socket. One maker misses the window; its quote simply never exists.",
        cells: [["QUOTES RETURNED", "5 OF 6", ""], ["NO RESPONSE", "1", "hold"], ["BEST PREMIUM", "0.1420", ""]],
        elapsed: 1760
      },
      accept: {
        fig: "[ STAGE 3 / ACCEPT ]",
        desc: "You take the best quote. Its signed bytes embed in your transaction, so the price you saw is the only price that can execute.",
        cells: [["QUOTE ACCEPTED", "0.1420", ""], ["SIZE", "25", ""], ["PREMIUM DUE", "3.5500", ""]],
        elapsed: 2340
      },
      settle: {
        fig: "[ STAGE 4 / SETTLE ]",
        desc: "The chain re-verifies the signature and executes atomically: collateral locks, option coins mint, premium pays — or it all reverts.",
        cells: [["COLLATERAL LOCKED", "25 SUI", ""], ["OPTION COINS MINTED", "25", ""], ["PREMIUM PAID", "3.5500", ""]],
        elapsed: 4100
      }
    };
    var TOTAL = 4100;
    var ORDER = ["request", "quotes", "accept", "settle"];
    var root = document.querySelector(".stage");
    if (!root) return;
    var pins = Array.prototype.slice.call(root.querySelectorAll(".stage__pin"));
    var fig = root.querySelector(".stage__fig");
    var desc = root.querySelector(".stage__desc");
    var cellsBox = root.querySelector(".stage__cells");
    var bar = root.querySelector(".stage__progress i");
    var elapsed = root.querySelector(".stage__elapsed-line");
    var pct = root.querySelector(".stage__pct");
    function fmt(n) { return String(n).replace(/\\B(?=(\\d{3})+(?!\\d))/g, ","); }
    function render(value) {
      var s = STAGES[value];
      root.setAttribute("data-stage", value);
      pins.forEach(function (p) {
        var on = p.getAttribute("data-value") === value;
        p.setAttribute("aria-checked", String(on));
        p.tabIndex = on ? 0 : -1;
      });
      fig.textContent = s.fig;
      desc.textContent = s.desc;
      cellsBox.innerHTML = s.cells.map(function (c) {
        return '<div class="stage__cell' + (c[2] === "hold" ? " stage__cell--hold" : "") +
          '"><dt>' + c[0] + "</dt><dd>" + c[1] + "</dd></div>";
      }).join("");
      var share = Math.round((s.elapsed / TOTAL) * 100);
      bar.style.width = share + "%";
      elapsed.textContent = "ELAPSED " + fmt(s.elapsed) + " OF " + fmt(TOTAL) + " MS";
      pct.textContent = share + "% OF RUN";
    }
    root.querySelector(".stage__pins").addEventListener("click", function (e) {
      var pin = e.target.closest(".stage__pin");
      var value = pin
        ? pin.getAttribute("data-value")
        : ORDER[(ORDER.indexOf(root.getAttribute("data-stage")) + 1) % ORDER.length];
      render(value);
      (pin || root.querySelector('.stage__pin[data-value="' + value + '"]')).focus();
    });
    root.querySelector(".stage__pins").addEventListener("keydown", function (e) {
      var i = ORDER.indexOf(root.getAttribute("data-stage"));
      var next = null;
      if (e.key === "ArrowRight" || e.key === "ArrowDown") next = ORDER[(i + 1) % ORDER.length];
      if (e.key === "ArrowLeft" || e.key === "ArrowUp") next = ORDER[(i + ORDER.length - 1) % ORDER.length];
      if (next) {
        e.preventDefault();
        render(next);
        var pin = root.querySelector('.stage__pin[data-value="' + next + '"]');
        if (pin) pin.focus();
      }
    });
  })();

  /* ---------- queue-status-filter: the flow ledger ---------- */
  (function () {
    var FLOWS = [
      ["F-01", "collateral locks, option coins mint 1:1", "options"],
      ["F-02", "premium pays the writer instantly", "options"],
      ["F-03", "FIFO cursor assigns exercises — no lottery", "options"],
      ["F-04", "offset closure frees collateral mid-cycle", "options"],
      ["F-05", "spread compression escrows the long call", "options"],
      ["F-06", "maker posts signed orders over HTTP, free", "exchange"],
      ["F-07", "fill re-verifies the signature on-chain", "exchange"],
      ["F-08", "router flow hits maker quotes atomically", "exchange"],
      ["F-09", "owner withdrawal works even when paused", "exchange"],
      ["F-10", "deposits mint shares, per-user cost basis", "vaults"],
      ["F-11", "one pool quotes both venues at once", "vaults"],
      ["F-12", "every venue path returns funds to the vault", "vaults"]
    ];
    var root = document.querySelector(".ledger");
    if (!root) return;
    var chips = Array.prototype.slice.call(root.querySelectorAll(".ledger__chip"));
    var log = root.querySelector(".ledger__log");
    var tally = root.querySelector(".ledger__tally");
    function rowsFor(value) {
      return value === "all" ? FLOWS : FLOWS.filter(function (f) { return f[2] === value; });
    }
    chips.forEach(function (chip) {
      chip.querySelector("b").textContent = String(rowsFor(chip.getAttribute("data-value")).length);
    });
    function render(value) {
      var rows = rowsFor(value);
      root.setAttribute("data-filter", value);
      chips.forEach(function (c) { c.setAttribute("aria-pressed", String(c.getAttribute("data-value") === value)); });
      log.innerHTML = rows.map(function (f) {
        return '<li class="ledger__row" data-flow="' + f[2] + '"><span>' + f[0] + "</span><em>" + f[1] +
          "</em><i></i><b>" + f[2].toUpperCase() + "</b></li>";
      }).join("");
      tally.textContent = "SHOWING " + rows.length + " OF " + FLOWS.length + " FLOWS · " +
        ((rows.length / FLOWS.length) * 100).toFixed(1) + "%";
    }
    var CHIP_ORDER = ["all", "options", "exchange", "vaults"];
    root.querySelector(".ledger__chips").addEventListener("click", function (e) {
      var chip = e.target.closest(".ledger__chip");
      var value = chip
        ? chip.getAttribute("data-value")
        : CHIP_ORDER[(CHIP_ORDER.indexOf(root.getAttribute("data-filter")) + 1) % CHIP_ORDER.length];
      render(value);
    });
  })();

  /* ---------- benchmark-contour-tabs: capital efficiency ---------- */
  (function () {
    var root = document.querySelector(".capital__tabs");
    if (!root) return;
    var VALUES = ["requote", "venues", "backing"];
    var tabs = Array.prototype.slice.call(root.querySelectorAll(".capital__tab"));
    var panels = {};
    VALUES.forEach(function (v) { panels[v] = document.getElementById("cap-panel-" + v); });
    function render(value) {
      root.setAttribute("data-metric", value);
      tabs.forEach(function (t) {
        var on = t.getAttribute("data-value") === value;
        t.setAttribute("aria-selected", String(on));
        t.tabIndex = on ? 0 : -1;
      });
      VALUES.forEach(function (v) { panels[v].hidden = v !== value; });
    }
    root.addEventListener("click", function (e) {
      var tab = e.target.closest(".capital__tab");
      var value = tab
        ? tab.getAttribute("data-value")
        : VALUES[(VALUES.indexOf(root.getAttribute("data-metric")) + 1) % VALUES.length];
      render(value);
      (tab || root.querySelector('.capital__tab[data-value="' + value + '"]')).focus();
    });
    root.addEventListener("keydown", function (e) {
      var i = VALUES.indexOf(root.getAttribute("data-metric"));
      var next = null;
      if (e.key === "ArrowDown" || e.key === "ArrowRight") next = VALUES[(i + 1) % VALUES.length];
      if (e.key === "ArrowUp" || e.key === "ArrowLeft") next = VALUES[(i + VALUES.length - 1) % VALUES.length];
      if (next) {
        e.preventDefault();
        render(next);
        var tab = root.querySelector('.capital__tab[data-value="' + next + '"]');
        if (tab) tab.focus();
      }
    });
  })();

  /* ---------- deployment-record-disclosure: trust records ---------- */
  document.querySelectorAll(".trust-card").forEach(function (card) {
    var btn = card.querySelector(".trust-card__open");
    var rec = card.querySelector(".trust-card__rec");
    var state = card.querySelector(".trust-card__state");
    function set(open) {
      card.setAttribute("data-record", open ? "open" : "sealed");
      btn.setAttribute("aria-expanded", String(open));
      rec.hidden = !open;
      if (!open) { state.textContent = "SEALED"; return; }
      var can = rec.querySelectorAll('[data-kind="can"]').length;
      var cannot = rec.querySelectorAll('[data-kind="cannot"]').length;
      state.textContent = "OPEN · " + can + " POWERS · " + cannot + " HARD LIMITS";
    }
    btn.addEventListener("click", function () {
      set(btn.getAttribute("aria-expanded") !== "true");
    });
  });

  /* ---------- terrain feed rests on phones; its data-live goes with it ---------- */
  (function () {
    var mq = window.matchMedia("(max-width: 900px)");
    var section = document.querySelector(".capital");
    var feed = document.querySelector(".terrain-feed");
    if (!section || !feed) return;
    function apply() {
      if (mq.matches) {
        section.classList.add("is-mobile-still");
        feed.removeAttribute("data-live");
      } else {
        section.classList.remove("is-mobile-still");
        feed.setAttribute("data-live", "contour-terrain-plot");
      }
    }
    mq.addEventListener("change", apply);
    apply();
  })();

  }

  if (document.readyState === "complete") {
    setTimeout(init, 60);
  } else {
    window.addEventListener("load", function () { setTimeout(init, 60); });
  }
})();
`;
