import type { ReactNode } from "react";
import type { OfficeItem } from "../../../lib/office";

// Desk, chair, laptop, post-it and desk items as inline SVG (2.5D). Gradients and the blur filter
// come from the single OfficeDefs component in Workplace (ids office-*). Theme colours are CSS variables;
// hard-coded colours are theme-independent details (pens, ring binder, key lines).

export interface DeskArtProps {
  laptop: boolean;
  postit: boolean;
  chair: boolean;
  items: readonly OfficeItem[];
}

const ITEM_ART: Record<OfficeItem, ReactNode> = {
  mug: (
    <g transform="translate(72,12)">
      <ellipse cx="2" cy="3" rx="7" ry="2" fill="var(--dshadow)" opacity=".25" />
      <rect x="-5" y="-9" width="10" height="11" rx="1.5" fill="var(--mug)" />
      <path d="M5 -6 q5 0 5 3.5 q0 3.5 -5 3.5" fill="none" stroke="var(--mug2)" strokeWidth="1.6" />
      <ellipse cx="0" cy="-9" rx="5" ry="1.8" fill="var(--mug2)" />
      <ellipse cx="0" cy="-9" rx="3.4" ry="1.1" fill="#6b4a2d" opacity=".8" />
    </g>
  ),
  pens: (
    <g transform="translate(74,12)">
      <ellipse cx="2" cy="3" rx="6" ry="1.8" fill="var(--dshadow)" opacity=".25" />
      <line x1="-2.5" y1="-8" x2="-4.5" y2="-21" stroke="#2f6fdf" strokeWidth="1.6" strokeLinecap="round" />
      <line x1="0.5" y1="-8" x2="1" y2="-22" stroke="#c4312b" strokeWidth="1.6" strokeLinecap="round" />
      <line x1="3" y1="-8" x2="5.5" y2="-20" stroke="#d9a21b" strokeWidth="1.8" strokeLinecap="round" />
      <rect x="-5" y="-10" width="10" height="12" rx="1.5" fill="var(--cup)" />
      <ellipse cx="0" cy="-10" rx="5" ry="1.7" fill="var(--cab2)" />
      <line x1="-3" y1="-7" x2="-3" y2="0" stroke="#fff" strokeOpacity=".25" />
    </g>
  ),
  papers: (
    <g transform="translate(38,15)">
      <polygon points="-10,-4 11,-4 8,6 -13,6" fill="var(--paper2)" />
      <polygon points="-10,-6 11,-6 8,4 -13,4" fill="var(--paper)" />
      <g stroke="var(--muted)" strokeOpacity=".6" strokeWidth=".9" strokeLinecap="round">
        <line x1="-7" y1="-3" x2="5" y2="-3" />
        <line x1="-8" y1="0" x2="1" y2="0" />
      </g>
      <rect x="-8" y="-9" width="6" height="4" rx=".6" fill="#c4312b" opacity=".85" transform="skewX(-30)" />
    </g>
  ),
  plant: (
    <g transform="translate(72,13)">
      <ellipse cx="1" cy="3" rx="7" ry="2" fill="var(--dshadow)" opacity=".25" />
      <polygon points="-5,-6 5,-6 4,2 -4,2" fill="var(--pot)" />
      <rect x="-5.5" y="-7" width="11" height="2" rx=".6" fill="#a4744a" />
      <g fill="var(--leaf)">
        <path d="M0 -7 q-5 -4 -1 -11 q4 5 1 11z" />
        <path d="M0 -7 q5 -4 2 -11 q-5 4 -2 11z" />
        <path d="M0 -7 q-7 1 -8 -6 q6 -1 8 6z" fill="#6bb07f" />
        <path d="M0 -7 q7 1 8 -6 q-6 -1 -8 6z" fill="#3f8559" />
      </g>
    </g>
  ),
  lamp: (
    <g>
      <polygon points="118,22 152,10 164,12 146,26" fill="var(--lampc)" filter="url(#office-blur)" />
      <g transform="translate(157,7)">
        <ellipse cx="0" cy="0" rx="6" ry="2.2" fill="var(--leg)" />
        <path d="M0 0 L-5 -18 L-15 -27" fill="none" stroke="var(--leg)" strokeWidth="2" strokeLinecap="round" />
        <circle cx="-5" cy="-18" r="1.6" fill="var(--cab2)" />
        <g transform="translate(-15,-27) rotate(-38)">
          <path d="M-7 0 L7 0 L4.5 -8 L-4.5 -8 Z" fill="var(--cab2)" />
          <path d="M-7 0 L7 0 L5.5 1.8 L-5.5 1.8 Z" fill="#ffe9a8" opacity=".9" />
        </g>
      </g>
    </g>
  ),
};

export default function DeskArt({ laptop, postit, chair, items }: DeskArtProps) {
  return (
    <svg
      viewBox="0 -30 172 108"
      preserveAspectRatio="xMidYMax meet"
      aria-hidden="true"
      className="block h-auto w-full overflow-visible"
    >
      {/* shadow to the right/down */}
      <polygon
        points="16,40 158,16 172,26 150,82 24,82"
        fill="var(--dshadow)"
        opacity=".45"
        filter="url(#office-blur)"
      />
      {chair && (
        <>
          {/* office chair behind the desk: back, frame and seat (seat hidden by the desk top) */}
          <rect x="70" y="-26" width="46" height="34" rx="9" fill="var(--chair)" />
          <rect x="74" y="-22" width="38" height="26" rx="7" fill="var(--chair2)" opacity=".55" />
          <rect x="90" y="6" width="6" height="12" fill="var(--chair2)" />
        </>
      )}
      {/* back right leg */}
      <rect x="158" y="14" width="5" height="38" rx="1.5" fill="var(--leg)" />
      {/* desk top (parallelogram, rounded corners via stroke) */}
      <polygon
        points="30,2 164,2 142,28 4,28"
        fill="url(#office-top)"
        stroke="url(#office-top)"
        strokeWidth={4}
        strokeLinejoin="round"
      />
      <line x1="31" y1="2.2" x2="163" y2="2.2" stroke="#fff" strokeOpacity=".35" strokeWidth="1.2" />
      <path d="M14 20 Q70 16 128 18" fill="none" stroke="#fff" strokeOpacity=".12" strokeWidth="1" />
      {/* desk edge (thickness) front and right */}
      <rect x="2" y="28" width="142" height="6" rx="1" fill="var(--desk-edge)" />
      <polygon points="144,28 166,2 166,8 144,34" fill="var(--desk-edge2)" />
      {/* front panel and its side face */}
      <rect x="14" y="34" width="118" height="31" fill="url(#office-apron)" />
      <line x1="14" y1="35" x2="132" y2="35" stroke="#fff" strokeOpacity=".18" />
      <polygon points="132,34 141,24.5 141,55.5 132,65" fill="var(--apron2)" />
      {/* front legs */}
      <rect x="4" y="34" width="6" height="42" rx="1.5" fill="var(--leg)" />
      <rect x="136" y="34" width="6" height="42" rx="1.5" fill="var(--leg)" />
      <ellipse cx="7" cy="76" rx="6" ry="1.6" fill="var(--dshadow)" opacity=".35" />
      <ellipse cx="139" cy="76" rx="6" ry="1.6" fill="var(--dshadow)" opacity=".35" />
      {postit && (
        <g transform="translate(34,14) rotate(-7)">
          <polygon
            points="0,-6 22,-6 18,6 -4,6"
            fill="var(--note-bg)"
            stroke="var(--note-border)"
            strokeWidth=".8"
          />
          <line x1="3" y1="-2" x2="15" y2="-2" stroke="var(--note-fg)" strokeOpacity=".35" />
          <line x1="2" y1="2" x2="11" y2="2" stroke="var(--note-fg)" strokeOpacity=".35" />
        </g>
      )}
      {/* desk items ("more"): the laptop is drawn last, in front of them */}
      {items.map((k, i) => (k === "papers" && postit ? null : <g key={`${k}-${i}`}>{ITEM_ART[k]}</g>))}
      {laptop && (
        <>
          {/* screen glow on the desk (drawn per desk: --scr is set on the seat) */}
          <ellipse cx="118" cy="20" rx="28" ry="7" fill="var(--scr)" opacity=".3" filter="url(#office-blur)" />
          <polygon points="92,26 134,26 150,10 108,10" fill="url(#office-lap)" />
          <polygon points="92,26 134,26 134,28.5 92,28.5" fill="var(--lap2)" />
          <polygon points="98,23.5 129,23.5 140,13 109,13" fill="#0e1a26" opacity=".55" />
          <g stroke="#9fbfd1" strokeOpacity=".35" strokeWidth=".9">
            <line x1="103" y1="21" x2="131" y2="21" />
            <line x1="105" y1="18.5" x2="133" y2="18.5" />
            <line x1="107.5" y1="16" x2="135.5" y2="16" />
          </g>
          <polygon points="110,25 122,25 124,23 112,23" fill="#9fbfd1" opacity=".25" />
          <rect x="108" y="8.5" width="42" height="2.5" rx="1" fill="var(--lap2)" />
          <rect
            x="107"
            y="-20"
            width="44"
            height="30"
            rx="3"
            fill="var(--screen-bezel)"
            stroke="var(--screen-rim)"
            strokeWidth="1"
          />
          <rect x="110" y="-17" width="38" height="23" rx="1.5" fill="var(--scr)" />
          <g fill="#fff">
            <rect className="office-ln" x="113" y="-13" width="18" height="2" rx="1" opacity=".7" />
            <rect className="office-ln" x="113" y="-8" width="26" height="2" rx="1" opacity=".55" />
            <rect className="office-ln" x="113" y="-3" width="12" height="2" rx="1" opacity=".65" />
          </g>
          <rect x="110" y="-17" width="38" height="23" rx="1.5" fill="url(#office-gloss)" />
          <circle cx="129" cy="8.2" r=".9" fill="#9fbfd1" opacity=".6" />
        </>
      )}
    </svg>
  );
}
