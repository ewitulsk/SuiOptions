// A Lightweight Charts v5 series primitive that shades vertical time bands —
// the §3.2 (SO-418) regime backgrounds for CoverageBreach / Impaired /
// ResetPending windows on the trading-vault PPS chart.
//
// Same construction as `apyBandPrimitive`: the primitive maps band edges to
// pixel x-coordinates via the chart's time scale and fills full-height
// rectangles on the pane canvas beneath the series (zOrder "bottom"). An
// open-ended band (`to: null`) runs to the right edge; edges outside the
// visible range clamp to the pane borders instead of dropping the band.

import type {
  Coordinate,
  IChartApi,
  IPrimitivePaneRenderer,
  IPrimitivePaneView,
  ISeriesApi,
  ISeriesPrimitive,
  SeriesAttachedParameter,
  SeriesType,
  Time,
  UTCTimestamp,
} from "lightweight-charts";

export type RegimeBand = {
  /** Seconds (UTCTimestamp domain, matching the series data). */
  from: UTCTimestamp;
  /** Null = open-ended (shades to the right edge). */
  to: UTCTimestamp | null;
  color: string;
};

/** Minimal shape of the canvas target passed to a pane renderer's `draw`. */
interface BitmapTarget {
  useBitmapCoordinateSpace(
    cb: (scope: {
      context: CanvasRenderingContext2D;
      bitmapSize: { width: number; height: number };
      horizontalPixelRatio: number;
      verticalPixelRatio: number;
    }) => void,
  ): void;
}

class RegimePaneRenderer implements IPrimitivePaneRenderer {
  constructor(private readonly _src: RegimeShadePrimitive) {}

  draw(target: BitmapTarget): void {
    const chart = this._src.chart;
    const bands = this._src.bands;
    if (!chart || bands.length === 0) return;
    const timeScale = chart.timeScale();
    const visible = timeScale.getVisibleRange();
    if (!visible) return;
    const width = timeScale.width();
    const vFrom = visible.from as number;
    const vTo = visible.to as number;

    // timeToCoordinate returns null for off-screen times — clamp such edges
    // to the pane borders so a band spanning the view still shades it.
    const xFor = (t: number, edge: "from" | "to"): Coordinate | number | null => {
      const c = timeScale.timeToCoordinate(t as UTCTimestamp);
      if (c !== null) return c;
      if (edge === "from" && t <= vFrom) return 0;
      if (edge === "to" && t >= vTo) return width;
      return null;
    };

    target.useBitmapCoordinateSpace(({ context: ctx, bitmapSize, horizontalPixelRatio: hr }) => {
      for (const band of bands) {
        const toTime = band.to ?? (vTo as UTCTimestamp);
        if ((band.from as number) > vTo || (toTime as number) < vFrom) continue;
        const x1 = xFor(band.from as number, "from");
        const x2 = xFor(toTime as number, "to");
        if (x1 === null || x2 === null || x2 <= x1) continue;
        ctx.fillStyle = band.color;
        ctx.fillRect(x1 * hr, 0, (x2 - x1) * hr, bitmapSize.height);
      }
    });
  }
}

class RegimePaneView implements IPrimitivePaneView {
  private readonly _renderer: RegimePaneRenderer;
  constructor(src: RegimeShadePrimitive) {
    this._renderer = new RegimePaneRenderer(src);
  }
  // Beneath the price lines so the series stay crisp on top.
  zOrder(): "bottom" {
    return "bottom";
  }
  renderer(): IPrimitivePaneRenderer {
    return this._renderer;
  }
}

export class RegimeShadePrimitive implements ISeriesPrimitive<Time> {
  chart: IChartApi | null = null;
  series: ISeriesApi<SeriesType> | null = null;
  bands: RegimeBand[] = [];

  private readonly _paneViews: RegimePaneView[];
  private _requestUpdate?: () => void;

  constructor() {
    this._paneViews = [new RegimePaneView(this)];
  }

  attached(param: SeriesAttachedParameter<Time>): void {
    this.chart = param.chart;
    this.series = param.series;
    this._requestUpdate = param.requestUpdate;
  }

  detached(): void {
    this.chart = null;
    this.series = null;
    this._requestUpdate = undefined;
  }

  setBands(bands: RegimeBand[]): void {
    this.bands = bands;
    this._requestUpdate?.();
  }

  updateAllViews(): void {}

  paneViews(): readonly IPrimitivePaneView[] {
    return this._paneViews;
  }
}
