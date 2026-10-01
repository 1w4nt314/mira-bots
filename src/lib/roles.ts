// Role presentation: order, Danish labels and the figure name (mirrors `Role` in Rust).
import type { BotCoreRole } from "./botCore";
import type { Role } from "./types";

/** Display and wire order (mirrors `Role::ALL`). */
export const ROLE_ORDER: readonly Role[] = [
  "coder",
  "researcher",
  "reviewer",
  "coordinator",
  "planner",
  "debugger",
];

export const ROLE_LABEL: Record<Role, string> = {
  coder: "Koder",
  researcher: "Researcher",
  reviewer: "Reviewer",
  coordinator: "Koordinator",
  planner: "Planlægger",
  debugger: "Debugger",
};

/** Staff roles (mirrors `Role::is_staff`): a staff seat needs at least one of them. */
export const STAFF_ROLES: readonly Role[] = ["reviewer", "coordinator", "planner"];

/** Whether the roles include a staff role (mirrors `has_staff_role` in Rust). */
export function hasStaffRole(roles: readonly Role[]): boolean {
  return roles.some((r) => STAFF_ROLES.includes(r));
}

/** Figure (file and generator) name of a role: the coordinator's is `koord`. */
export function figureNameFor(role: Role): BotCoreRole {
  return role === "coordinator" ? "koord" : role;
}

/** Roles deduplicated in `ROLE_ORDER` (like `normalize` in Rust). */
export function sortRoles(roles: readonly Role[]): Role[] {
  return ROLE_ORDER.filter((r) => roles.includes(r));
}

/** "Koder · Reviewer", or `none` without roles. */
export function rolesText(roles: readonly Role[], none = "Ingen roller"): string {
  return roles.length === 0 ? none : sortRoles(roles).map((r) => ROLE_LABEL[r]).join(" · ");
}

/** A profile's `specialist` resolved (null → `roles.length !== 1`, like `is_specialist`). */
export function isSpecialist(p: { roles: readonly Role[]; specialist: boolean | null }): boolean {
  return p.specialist ?? p.roles.length !== 1;
}

/**
 * Folder name prefix of an agent's default folder (mirrors `prefix_for` in Rust).
 *
 * | roles             | specialist | prefix       |
 * |-------------------|------------|--------------|
 * | [coordinator]     | false      | "koord"      |
 * | [coder]           | false      | "coder"      |
 * | []                | false      | "bot"        |
 * | [coder]           | true       | "specialist" |
 * | [coder, reviewer] | any        | "specialist" |
 */
export function folderPrefix(roles: readonly Role[], specialist: boolean): string {
  const r = sortRoles(roles);
  if (r.length === 1 && !specialist) return figureNameFor(r[0]);
  if (r.length === 0 && !specialist) return "bot";
  return "specialist";
}
