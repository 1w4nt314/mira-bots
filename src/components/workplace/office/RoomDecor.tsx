// Office furniture beside the staff glass office ("more" only): filing cabinet + bin on the left,
// water cooler + plant on the right. Purely decorative; height follows the desk height (CSS).
export default function RoomDecor({ side }: { side: "left" | "right" }) {
  return (
    <div className={`office-decor office-decor-${side}`} aria-hidden="true">
      {side === "left" ? (
        <>
          <svg viewBox="0 0 58 72">
            <polygon
              points="8,70 50,66 58,56 48,74 14,76"
              fill="var(--dshadow)"
              opacity=".3"
              filter="url(#office-blur)"
            />
            <polygon points="6,16 40,16 52,4 18,4" fill="var(--cab3)" />
            <polygon points="40,16 52,4 52,56 40,68" fill="var(--cab2)" />
            <rect x="6" y="16" width="34" height="52" fill="var(--cab)" />
            <g fill="none" stroke="var(--cab2)" strokeWidth="1">
              <rect x="9.5" y="19.5" width="27" height="21" />
              <rect x="9.5" y="43.5" width="27" height="21" />
            </g>
            <g fill="var(--cab3)">
              <rect x="18" y="29" width="10" height="2.4" rx="1" />
              <rect x="18" y="53" width="10" height="2.4" rx="1" />
            </g>
            <g>
              <rect x="22" y="-2" width="14" height="6" rx="1" fill="#d9a21b" transform="skewX(-30) translate(8 0)" />
              <rect x="22" y="-7" width="12" height="5" rx="1" fill="#2f6fdf" transform="skewX(-30) translate(6 0)" />
            </g>
          </svg>
          <svg viewBox="0 0 30 44">
            <polygon points="6,12 24,12 22,40 8,40" fill="var(--bin)" />
            <g stroke="#fff" strokeOpacity=".18" strokeWidth="1">
              <line x1="10.5" y1="14" x2="11.2" y2="38" />
              <line x1="15" y1="14" x2="15" y2="38" />
              <line x1="19.5" y1="14" x2="18.8" y2="38" />
            </g>
            <ellipse cx="15" cy="12" rx="9" ry="2.6" fill="var(--cab2)" />
            <ellipse cx="15" cy="12" rx="7" ry="1.8" fill="#1d2127" opacity=".45" />
            <path d="M11 10 l3 -4 l3 4" fill="var(--paper)" opacity=".8" />
          </svg>
        </>
      ) : (
        <>
          <svg viewBox="0 0 44 96">
            <ellipse cx="24" cy="93" rx="16" ry="3" fill="var(--dshadow)" opacity=".3" />
            <rect x="10" y="38" width="24" height="52" rx="3" fill="var(--cooler)" />
            <rect x="10" y="80" width="24" height="10" rx="2" fill="var(--cooler2)" />
            <rect x="13" y="50" width="18" height="12" rx="1.5" fill="var(--cooler2)" />
            <rect x="15" y="55" width="4" height="5" rx="1" fill="#2f6fdf" />
            <rect x="25" y="55" width="4" height="5" rx="1" fill="#c4312b" />
            <rect x="14" y="34" width="16" height="5" rx="1" fill="var(--cooler2)" />
            <path d="M12 30 q0 -26 10 -26 q10 0 10 26 z" fill="var(--water)" opacity=".85" />
            <rect x="18" y="2" width="8" height="4" rx="1" fill="var(--water)" />
            <path d="M16 26 q-1 -14 4 -18" fill="none" stroke="#fff" strokeOpacity=".55" strokeWidth="2" strokeLinecap="round" />
            <rect x="33" y="60" width="5" height="9" rx="1" fill="var(--cooler2)" />
            <rect x="34" y="62" width="3" height="6" fill="#fff" opacity=".7" />
          </svg>
          <svg viewBox="0 0 44 72">
            <ellipse cx="22" cy="69" rx="14" ry="3" fill="var(--dshadow)" opacity=".3" />
            <path d="M22 48 C12 42 7 32 9 20 c8 3 13 12 13 28z" fill="var(--leaf)" />
            <path d="M22 48 c10 -6 15 -16 13 -28 -8 3 -13 12 -13 28z" fill="#3f8559" />
            <path d="M22 50 C18 40 20 28 24 20" fill="none" stroke="#2f6b45" strokeWidth="1.6" />
            <path d="M22 46 C14 44 10 36 11 30 c6 2 10 8 11 16z" fill="#6bb07f" />
            <path d="M11 48 h22 l-2.5 20 h-17z" fill="var(--pot)" />
            <path d="M10 47 h24 v4 h-24z" fill="#a4744a" />
          </svg>
        </>
      )}
    </div>
  );
}
