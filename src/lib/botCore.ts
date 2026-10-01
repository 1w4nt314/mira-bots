// Port of src/assets/bots/bot-core.reference.js (the figure generator, kept as the test oracle):
// `renderBot` returns, character for character, the same SVG string as the reference's
// `bot(st, roles, dark, spec, id)` (checked by scripts/test-bot-core.mjs for 160 combinations).
// Pure: no DOM, no imports. Only known role names and our own `id` ever reach the SVG; profile
// names, titles and other user text never do (the aria-label is built from fixed role names).

/** Figure state (the reference's `st`). */
export type BotCoreState = "idle" | "work" | "wait" | "done";

/** Role names as the generator knows them (`koord` = coordinator). */
export type BotCoreRole = "coder" | "researcher" | "reviewer" | "koord" | "planner" | "debugger";

export interface RenderBotInput {
  state: BotCoreState;
  roles: readonly BotCoreRole[];
  dark: boolean;
  specialist: boolean;
  /** clipPath id inside the SVG; must be a plain identifier (callers pass a constant). */
  id: string;
}

const SAND = "#CBA678",
  SAND2 = "#A98757",
  NIGHT = "#152433",
  DEEP = "#0B1520",
  BIRCH = "#F5F3EE",
  ICE = "#9FBFD1",
  MID = "#213649",
  PANEL = "#1C3043",
  DARKP = "#0F1C29";
const FONT = "system-ui,-apple-system,'Segoe UI',Roboto,sans-serif";
const RN: Record<BotCoreRole | "none" | "specialist", string> = {
  none: "ingen rolle",
  coder: "coder",
  researcher: "researcher",
  reviewer: "reviewer",
  koord: "koordinator",
  planner: "planner",
  debugger: "debugger",
  specialist: "specialist",
};
const CSS = `.scan{animation:scan 1.6s ease-in-out infinite}.bob{animation:bob .9s ease-in-out infinite}.pulse{animation:pulse 1.1s ease-in-out infinite;transform-box:fill-box;transform-origin:center}.zz{animation:zz 2s ease-in-out infinite}.d1,.d2,.d3{animation:dd 1.2s ease-in-out infinite}.d2{animation-delay:.2s}.d3{animation-delay:.4s}.tL{animation:t1 .7s ease-in-out infinite}.tR{animation:t2 .7s ease-in-out infinite}.wL{animation:wl .8s ease-in-out infinite}.wR{animation:wr .8s ease-in-out infinite}.lA{animation:ld .7s ease-in-out infinite}.lB{animation:ld .7s ease-in-out infinite;animation-delay:-.35s}.lP{animation:ld 1s ease-in-out infinite}.prog{animation:prog 1.6s ease-in-out infinite}.orb{animation:orb 9s linear infinite}@keyframes scan{0%,100%{transform:translateX(-4px)}50%{transform:translateX(4px)}}@keyframes bob{0%,100%{transform:translateY(0)}50%{transform:translateY(-3px)}}@keyframes pulse{0%,100%{transform:scale(1)}50%{transform:scale(1.3)}}@keyframes zz{0%,100%{opacity:.35}50%{opacity:1}}@keyframes dd{0%,100%{opacity:.25}50%{opacity:1}}@keyframes t1{0%,100%{transform:rotate(14deg)}50%{transform:rotate(32deg)}}@keyframes t2{0%,100%{transform:rotate(-32deg)}50%{transform:rotate(-14deg)}}@keyframes wl{0%,100%{transform:rotate(148deg)}50%{transform:rotate(168deg)}}@keyframes wr{0%,100%{transform:rotate(-148deg)}50%{transform:rotate(-168deg)}}@keyframes ld{0%,100%{opacity:1}50%{opacity:.2}}@keyframes prog{0%{transform:scaleX(.1)}80%,100%{transform:scaleX(1)}}@keyframes orb{to{transform:rotate(360deg)}}@media (prefers-reduced-motion:reduce){*{animation:none!important}}`;

const dia = (x: number, y: number, s: number, cls: string, stroke?: boolean): string =>
  `<path class="${cls || ""}" d="M${x} ${y - s}L${x + s * 0.75} ${y}L${x} ${y + s}L${x - s * 0.75} ${y}Z" fill="${SAND}"${stroke ? ` stroke="${NIGHT}" stroke-width="1.6"` : ""}/>`;
const arm = (side: "L" | "R", cls: string, ang: number, items: string): string =>
  `<g transform="translate(${side === "L" ? 48 : 192},170)"><g class="${cls}" transform="rotate(${ang})" style="transform-origin:0 0"><rect x="-7" y="-4" width="14" height="46" rx="7" fill="${DARKP}" stroke="${ICE}" stroke-opacity=".3"/><circle cx="0" cy="46" r="8.5" fill="#2B4257" stroke="${ICE}" stroke-opacity=".35"/>${items || ""}</g><circle r="9" fill="${MID}" stroke="${ICE}" stroke-opacity=".35"/></g>`;
const clipG = (r: number): string =>
  `<g transform="translate(0,46) rotate(${r})"><rect x="-30" y="-46" width="30" height="40" rx="4" fill="${BIRCH}" stroke="${SAND}" stroke-width="2.5"/><rect x="-21" y="-50" width="12" height="8" rx="2" fill="${SAND}"/><path d="M-25 -34L-22 -30L-16 -38M-25 -22L-22 -18L-16 -26" fill="none" stroke="#1D9E75" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"/><path d="M-12 -34L-4 -34M-12 -22L-4 -22M-24 -10L-6 -10" stroke="#4A5A66" stroke-width="2" stroke-linecap="round"/></g>`;
const lensG = (r: number): string =>
  `<g transform="translate(0,46) rotate(${r})"><path d="M0 -2L0 -18" stroke="${SAND}" stroke-width="5" stroke-linecap="round"/><circle cx="0" cy="-33" r="15" fill="${ICE}" fill-opacity=".4" stroke="${SAND}" stroke-width="4"/><path d="M-8 -40Q-4 -45 2 -44" fill="none" stroke="#fff" stroke-opacity=".6" stroke-width="2.5" stroke-linecap="round"/></g>`;
const rollG = (r: number): string =>
  `<g transform="translate(0,46) rotate(${r})"><rect x="-9" y="-50" width="18" height="44" rx="3" fill="${ICE}"/><path d="M-9 -42L9 -42M-9 -26L9 -26M-9 -14L9 -14" stroke="${NIGHT}" stroke-opacity=".22" stroke-width="1.3"/><ellipse cx="0" cy="-6" rx="9" ry="3.5" fill="#7FA3B8"/><ellipse cx="0" cy="-50" rx="9" ry="3.5" fill="${BIRCH}" stroke="${SAND}" stroke-width="1.6"/><ellipse cx="0" cy="-50" rx="4" ry="1.5" fill="none" stroke="${SAND2}" stroke-width="1.2"/><rect x="-10" y="-38" width="20" height="4.5" rx="1" fill="${SAND}"/><rect x="-10" y="-20" width="20" height="4.5" rx="1" fill="${SAND}"/></g>`;
const wrenchG = (r: number): string =>
  `<g transform="translate(0,46) rotate(${r})"><rect x="-4" y="-34" width="8" height="32" rx="4" fill="${SAND}"/><path d="M-12 -52L-4.5 -52L-4.5 -46L4.5 -46L4.5 -52L12 -52L12 -42C12 -36 8 -33 0 -33C-8 -33 -12 -36 -12 -42Z" fill="${SAND}"/></g>`;
const ORB = [SAND, ICE, BIRCH, "#1D9E75", "#BA7517"];

/** The figure as an SVG document string (ends with "\n", like the reference). */
export function renderBot({ state: st, roles, dark, specialist: spec, id }: RenderBotInput): string {
  const has = (r: BotCoreRole) => roles.includes(r);
  const wait = st === "wait",
    work = st === "work",
    done = st === "done",
    idle = st === "idle";
  const F = `font-family="${FONT}"`;
  let eyes = "",
    o = "";
  [98, 142].forEach((x) => {
    const ey = 122;
    if (done) {
      eyes += `<path d="M${x - 11} ${ey + 5}Q${x} ${ey - 13} ${x + 11} ${ey + 5}" fill="none" stroke="${ICE}" stroke-width="5" stroke-linecap="round"/>`;
      return;
    }
    const h = idle ? 5 : wait ? 30 : 26,
      w = 22,
      c = wait ? SAND : ICE;
    eyes += `<rect x="${x - w / 2 - 4}" y="${ey - h / 2 - 4}" width="${w + 8}" height="${h + 8}" rx="${Math.min(9, h / 2 + 4)}" fill="${c}" opacity=".16"/><rect x="${x - w / 2}" y="${ey - h / 2}" width="${w}" height="${h}" rx="${Math.min(6, h / 2)}" fill="${c}"/>`;
    if (work)
      eyes += `<rect class="scan" x="${x - 6}" y="${ey - 6}" width="12" height="12" rx="2.5" fill="${DEEP}"/><rect x="${x - w / 2 + 3}" y="${ey - h / 2 + 3}" width="6" height="4" rx="2" fill="#fff" opacity=".55"/>`;
  });
  if (wait)
    eyes += `<path d="M86 104L107 97M154 104L133 97" fill="none" stroke="${SAND}" stroke-width="3.5" stroke-linecap="round"/>`;
  const mouth = idle
    ? `<path d="M112 143L128 143" stroke="${ICE}" stroke-opacity=".6" stroke-width="3" stroke-linecap="round"/>`
    : work
      ? `<rect x="102" y="140" width="36" height="6" rx="3" fill="${ICE}" opacity=".22"/><rect class="prog" x="102" y="140" width="36" height="6" rx="3" fill="${ICE}" style="transform-origin:102px 143px"/>`
      : wait
        ? `<circle cx="120" cy="143" r="5" fill="none" stroke="${SAND}" stroke-width="3"/>`
        : `<path d="M106 138Q120 152 134 138" fill="none" stroke="${ICE}" stroke-width="4" stroke-linecap="round"/>`;
  o += `<ellipse cx="120" cy="233" rx="62" ry="7" fill="#000" opacity="${dark ? 0.4 : 0.12}"/>`;
  [90, 136].forEach((p) => {
    o += `<rect x="${p}" y="196" width="14" height="20" rx="5" fill="${DARKP}" stroke="${ICE}" stroke-opacity=".25"/>`;
  });
  o += `<rect x="78" y="212" width="38" height="16" rx="8" fill="${DARKP}" stroke="${ICE}" stroke-opacity=".3"/><rect x="124" y="212" width="38" height="16" rx="8" fill="${DARKP}" stroke="${ICE}" stroke-opacity=".3"/>`;
  o += `<g class="${work ? "bob" : ""}">`;
  if (has("coder"))
    o += `<path d="M48 104Q120 4 192 104" fill="none" stroke="${SAND}" stroke-width="6" stroke-linecap="round"/>`;
  o += `<rect x="118" y="40" width="4" height="36" rx="2" fill="${ICE}"/><rect x="114" y="54" width="12" height="4" rx="2" fill="${MID}"/><rect x="114" y="64" width="12" height="4" rx="2" fill="${MID}"/><rect x="106" y="70" width="28" height="9" rx="4" fill="${DARKP}" stroke="${ICE}" stroke-opacity=".25"/>`;
  if (has("planner"))
    o += `<path d="M122 42L146 49L122 56Z" fill="${SAND}" stroke="${NIGHT}" stroke-width="1.2" stroke-linejoin="round"/>`;
  o += `<rect x="42" y="104" width="12" height="44" rx="6" fill="${DARKP}" stroke="${ICE}" stroke-opacity=".25"/><rect x="186" y="104" width="12" height="44" rx="6" fill="${DARKP}" stroke="${ICE}" stroke-opacity=".25"/><circle cx="48" cy="114" r="2.2" fill="${ICE}" opacity=".8"/><circle cx="48" cy="124" r="2.2" fill="${ICE}" opacity=".4"/><circle cx="192" cy="114" r="2.2" fill="${ICE}" opacity=".8"/><circle cx="192" cy="124" r="2.2" fill="${ICE}" opacity=".4"/>`;
  o += `<rect x="50" y="74" width="140" height="128" rx="38" fill="${NIGHT}" stroke="${ICE}" stroke-opacity=".35" stroke-width="1.5"/><path d="M80 82Q120 76 160 82" fill="none" stroke="#fff" stroke-opacity=".12" stroke-width="3" stroke-linecap="round"/><path d="M58 164L182 164" stroke="#fff" stroke-opacity=".07" stroke-width="1.5"/><circle cx="62" cy="174" r="2.4" fill="${ICE}" opacity=".5"/><circle cx="178" cy="174" r="2.4" fill="${ICE}" opacity=".5"/>`;
  if (has("debugger")) {
    if (!idle) o += `<path d="M84 64L46 22L72 8L100 62Z" fill="${SAND}" opacity=".22"/>`;
    o += `<rect x="76" y="62" width="28" height="15" rx="7" fill="${SAND}"/><circle cx="90" cy="69.5" r="5.5" fill="${DEEP}"/><circle cx="90" cy="69.5" r="3.4" fill="${idle ? SAND2 : "#fff"}"/>`;
  }
  o += `<clipPath id="${id}"><rect x="70" y="94" width="100" height="60" rx="17"/></clipPath><rect x="64" y="88" width="112" height="72" rx="22" fill="${DEEP}" stroke="${ICE}" stroke-opacity=".5" stroke-width="2"/><g clip-path="url(#${id})"><rect x="70" y="94" width="100" height="60" fill="#07111B"/>`;
  for (let y = 98; y < 154; y += 6)
    o += `<rect x="70" y="${y}" width="100" height="1" fill="${ICE}" opacity=".06"/>`;
  o += `<path d="M70 94L118 94L88 154L70 154Z" fill="#fff" opacity=".05"/>${eyes}${mouth}</g>`;
  if (has("reviewer")) {
    o +=
      [98, 142]
        .map(
          (x) =>
            `<circle cx="${x}" cy="122" r="21" fill="${ICE}" fill-opacity=".08" stroke="${SAND}" stroke-width="4"/><path d="M${x - 10} 108Q${x - 4} 104 ${x + 2} 106" fill="none" stroke="#fff" stroke-opacity=".4" stroke-width="2.5" stroke-linecap="round"/>`,
        )
        .join("") +
      `<path d="M77 120L67 116M163 120L173 116" stroke="${SAND}" stroke-width="3" stroke-linecap="round"/>`;
  }
  const led = idle
    ? ["#3A5670", "#3A5670", ""]
    : work
      ? ["#1D9E75", "#1D9E75", ""]
      : wait
        ? [SAND, SAND, "lP"]
        : ["#639922", "#639922", ""];
  const k = has("koord");
  const ps = k ? SAND : spec ? SAND : ICE,
    po = k ? 1 : spec ? 0.7 : 0.2,
    pw = k || spec ? 2.5 : 1;
  o += `<rect x="82" y="166" width="76" height="30" rx="11" fill="${PANEL}" stroke="${ps}" stroke-opacity="${po}" stroke-width="${pw}"/><circle class="${work ? "lA" : led[2]}" cx="97" cy="181" r="5" fill="${led[0]}"/><circle class="${work ? "lB" : led[2]}" cx="143" cy="181" r="5" fill="${led[1]}"/>`;
  if (spec)
    o +=
      `<rect x="104" y="174" width="32" height="14" rx="5" fill="${DEEP}"/>` +
      ORB.map((c, i) => `<circle cx="${109 + i * 5.5}" cy="181" r="2.1" fill="${c}"/>`).join("");
  else if (k) o += dia(120, 181, 18, wait ? "pulse" : "", true);
  else if (has("coder"))
    o += `<rect x="104" y="174" width="32" height="14" rx="5" fill="${DEEP}"/><text x="120" y="185" text-anchor="middle" font-size="12" fill="${SAND}" ${F}>&lt;/&gt;</text>`;
  else if (has("planner"))
    o += `<rect x="102" y="174" width="36" height="14" rx="5" fill="${DEEP}"/><path d="M108 184L118 178L127 184" fill="none" stroke="${SAND}" stroke-width="1.8" stroke-linejoin="round" stroke-linecap="round"/><circle cx="108" cy="184" r="2.6" fill="${SAND}"/><circle cx="118" cy="178" r="2.6" fill="${SAND}"/><path d="M127 184L127 176" stroke="${SAND}" stroke-width="1.8" stroke-linecap="round"/><path d="M127 176L133 178.5L127 181Z" fill="${SAND}"/>`;
  else if (has("debugger"))
    o += `<rect x="104" y="174" width="32" height="14" rx="5" fill="${DEEP}"/><ellipse cx="120" cy="182.5" rx="4.2" ry="5.4" fill="${SAND}"/><circle cx="120" cy="176.5" r="2.4" fill="${SAND}"/><path d="M115.5 180L110 177.5M115.5 183L109.5 183.5M116 185.5L111 189M124.5 180L130 177.5M124.5 183L130.5 183.5M124 185.5L129 189M119 174.5L117 171.5M121 174.5L123 171.5" fill="none" stroke="${SAND}" stroke-width="1.5" stroke-linecap="round"/><path d="M120 178L120 188" stroke="${DEEP}" stroke-width="1"/>`;
  else o += `<rect x="106" y="177" width="28" height="8" rx="4" fill="${DEEP}"/>`;
  if (has("coder"))
    o += `<rect x="34" y="98" width="18" height="40" rx="9" fill="${SAND}"/><rect x="38" y="105" width="8" height="26" rx="4" fill="${SAND2}"/><rect x="188" y="98" width="18" height="40" rx="9" fill="${SAND}"/><rect x="194" y="105" width="8" height="26" rx="4" fill="${SAND2}"/><path d="M43 136Q42 162 70 162" fill="none" stroke="${SAND}" stroke-width="4" stroke-linecap="round"/><circle cx="75" cy="162" r="6" fill="${SAND}"/><circle cx="75" cy="162" r="2.6" fill="${SAND2}"/>`;
  const L: [string, number] = done ? ["wL", 148] : work ? ["tL", 16] : ["", 8],
    R: [string, number] = done || wait ? ["wR", -148] : work ? ["tR", -16] : ["", -8];
  const left =
    (has("planner") ? rollG(has("reviewer") ? -48 : -35) : "") + (has("reviewer") ? clipG(-12) : "");
  const right =
    (has("researcher") ? lensG(has("debugger") ? 52 : 35) : "") +
    (has("debugger") ? wrenchG(has("researcher") ? 14 : 35) : "");
  o += arm("L", L[0], L[1], left) + arm("R", R[0], R[1], right);
  if (spec) {
    o +=
      `<g class="orb" style="transform-origin:120px 30px">` +
      ORB.map((c, i) => {
        const a = ((i * 72 - 90) * Math.PI) / 180;
        return `<circle cx="${(120 + 25 * Math.cos(a)).toFixed(1)}" cy="${(30 + 25 * Math.sin(a)).toFixed(1)}" r="3.2" fill="${c}" stroke="${NIGHT}" stroke-opacity=".45" stroke-width="1"/>`;
      }).join("") +
      `</g>`;
  }
  o +=
    `<circle class="${wait ? "pulse" : ""}" cx="120" cy="30" r="17" fill="none" stroke="${SAND}" stroke-width="2" opacity="${wait ? 0.8 : 0.3}"/>` +
    dia(120, 30, 11, wait ? "pulse" : "");
  o += "</g>";
  if (idle)
    o += `<g class="zz"><text x="166" y="50" font-size="22" fill="#888780" ${F}>z</text><text x="190" y="28" font-size="16" fill="#888780" ${F}>z</text></g>`;
  if (work)
    o += `<circle class="d1" cx="170" cy="34" r="4.5" fill="#1D9E75"/><circle class="d2" cx="184" cy="34" r="4.5" fill="#1D9E75"/><circle class="d3" cx="198" cy="34" r="4.5" fill="#1D9E75"/>`;
  if (wait)
    o += `<rect x="150" y="6" width="76" height="34" rx="13" fill="${SAND}"/><path d="M162 39L152 54L178 39Z" fill="${SAND}"/><text x="188" y="29" text-anchor="middle" font-size="15" font-weight="500" fill="${NIGHT}" ${F}>Allow?</text>`;
  if (done)
    o += `<circle cx="198" cy="38" r="17" fill="#639922"/><path d="M189 38L196 45L208 30" fill="none" stroke="#fff" stroke-width="4" stroke-linecap="round" stroke-linejoin="round"/>`;
  const sname = { idle: "idle", work: "arbejder", wait: "venter på lov", done: "færdig" }[st];
  const t = `Bot, ${sname}, ${
    spec
      ? "specialist" + (roles.length ? " med " + roles.map((r) => RN[r]).join(", ") : "")
      : RN[roles[0] || "none"]
  }`;
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 240 250" width="240" height="250" role="img" aria-label="${t}"><title>${t}</title><style>${CSS}</style>${o}</svg>\n`;
}
