import { useEffect, useState } from "react";

// Wall strip above the office ("more" only): three windows, wall clock (local time), picture,
// shelf with ring binders and a kanban whiteboard. Purely decorative.
export default function WallDecor() {
  const [now, setNow] = useState(() => new Date());
  // TODO(windows-verify): D.70 (clock shows Windows' local time and keeps ticking)
  useEffect(() => {
    const id = setInterval(() => setNow(new Date()), 20_000);
    return () => clearInterval(id);
  }, []);
  const hourDeg = (now.getHours() % 12) * 30 + now.getMinutes() / 2;
  const minuteDeg = now.getMinutes() * 6;

  return (
    <div className="office-wall" aria-hidden="true">
      <span className="office-win" />
      <span className="office-win" />
      <span className="office-win" />
      <svg viewBox="0 0 26 36">
        <circle cx="13" cy="17" r="10" fill="var(--board)" stroke="var(--leg)" strokeWidth="1.6" />
        <circle cx="13" cy="17" r="1" fill="var(--fg)" />
        <line
          x1="13"
          y1="17"
          x2="13"
          y2="11.5"
          stroke="var(--fg)"
          strokeWidth="1.6"
          strokeLinecap="round"
          transform={`rotate(${hourDeg} 13 17)`}
        />
        <line
          x1="13"
          y1="17"
          x2="13"
          y2="9.5"
          stroke="var(--fg)"
          strokeWidth="1.1"
          strokeLinecap="round"
          transform={`rotate(${minuteDeg} 13 17)`}
        />
      </svg>
      <svg viewBox="0 0 30 36">
        <rect x="2" y="7" width="26" height="22" rx="1.5" fill="var(--frame)" />
        <rect x="5" y="10" width="20" height="16" fill="#cfe3f0" />
        <path d="M5 26 L5 21 Q12 15 18 20 T25 19 L25 26Z" fill="#7fa66f" />
        <circle cx="20" cy="14" r="2" fill="#f2c94c" />
      </svg>
      <svg viewBox="0 0 76 36">
        <rect x="2" y="27" width="72" height="3" rx="1" fill="var(--desk-edge)" />
        <rect x="8" y="10" width="9" height="17" rx="1" fill="#c4312b" />
        <rect x="19" y="12" width="9" height="15" rx="1" fill="#2f6fdf" />
        <rect x="30" y="9" width="9" height="18" rx="1" fill="#d9a21b" />
        <rect x="41" y="12" width="9" height="15" rx="1" fill="#4f9a6a" />
        <rect x="55" y="16" width="14" height="11" rx="1" fill="var(--cab3)" transform="rotate(-8 62 27)" />
        <g fill="#fff" opacity=".7">
          <rect x="10" y="14" width="5" height="4" />
          <rect x="21" y="16" width="5" height="4" />
          <rect x="32" y="13" width="5" height="4" />
          <rect x="43" y="16" width="5" height="4" />
        </g>
      </svg>
      <svg viewBox="0 0 130 36">
        <rect x="2" y="5" width="126" height="26" rx="2" fill="var(--board)" stroke="var(--leg)" strokeWidth="1.2" />
        <g stroke="var(--leg)" strokeOpacity=".5" strokeWidth=".8">
          <line x1="44" y1="8" x2="44" y2="28" />
          <line x1="86" y1="8" x2="86" y2="28" />
        </g>
        <g stroke="var(--fg)" strokeOpacity=".55" strokeWidth="1.4" strokeLinecap="round">
          <line x1="8" y1="9" x2="26" y2="9" />
          <line x1="50" y1="9" x2="66" y2="9" />
          <line x1="92" y1="9" x2="104" y2="9" />
        </g>
        <g>
          <rect x="8" y="13" width="9" height="7" fill="#fff4b8" stroke="#e6d27a" strokeWidth=".6" />
          <rect x="20" y="14" width="9" height="7" fill="#fff4b8" stroke="#e6d27a" strokeWidth=".6" />
          <rect x="9" y="22" width="9" height="7" fill="#cfe3f0" stroke="#9fbfd1" strokeWidth=".6" />
          <rect x="50" y="14" width="9" height="7" fill="#fff4b8" stroke="#e6d27a" strokeWidth=".6" />
          <rect x="63" y="15" width="9" height="7" fill="#fbd0cd" stroke="#e4a09a" strokeWidth=".6" />
          <rect x="92" y="14" width="9" height="7" fill="#cde8d2" stroke="#8fc39b" strokeWidth=".6" />
          <rect x="104" y="15" width="9" height="7" fill="#cde8d2" stroke="#8fc39b" strokeWidth=".6" />
          <rect x="93" y="23" width="9" height="7" fill="#fff4b8" stroke="#e6d27a" strokeWidth=".6" />
        </g>
        <rect x="100" y="30" width="24" height="2.5" rx="1" fill="var(--cab2)" />
        <rect x="104" y="31" width="4" height="1.2" fill="#2f6fdf" />
        <rect x="110" y="31" width="4" height="1.2" fill="#c4312b" />
      </svg>
    </div>
  );
}
