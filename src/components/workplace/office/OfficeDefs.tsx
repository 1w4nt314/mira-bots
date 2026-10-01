// Shared SVG <defs> for the office look. Mounted exactly once (in Workplace), never display:none
// (a hidden defs-svg makes the gradients unresolvable in WebView2). Ids are fixed and prefixed
// "office-"; no gradient here may reference --scr (that variable lives on the seat, not here).
// TODO(windows-verify): D.63
export default function OfficeDefs() {
  return (
    <svg width="0" height="0" className="absolute h-0 w-0" aria-hidden="true" focusable="false">
      <defs>
        <linearGradient id="office-top" x1="0" y1="0" x2="1" y2="1">
          <stop offset="0" style={{ stopColor: "var(--desk-hi)" }} />
          <stop offset="1" style={{ stopColor: "var(--desk-lo)" }} />
        </linearGradient>
        <linearGradient id="office-apron" x1="0" y1="0" x2="0" y2="1">
          <stop offset="0" style={{ stopColor: "var(--apron)" }} />
          <stop offset="1" style={{ stopColor: "var(--apron2)" }} />
        </linearGradient>
        <linearGradient id="office-lap" x1="0" y1="0" x2="1" y2="1">
          <stop offset="0" style={{ stopColor: "var(--lap)" }} />
          <stop offset="1" style={{ stopColor: "var(--lap2)" }} />
        </linearGradient>
        <linearGradient id="office-gloss" x1="0" y1="0" x2="1" y2="1">
          <stop offset="0" stopColor="#fff" stopOpacity={0.28} />
          <stop offset="0.55" stopColor="#fff" stopOpacity={0} />
        </linearGradient>
        <filter id="office-blur" x="-20%" y="-20%" width="140%" height="160%">
          <feGaussianBlur stdDeviation="2.2" />
        </filter>
      </defs>
    </svg>
  );
}
