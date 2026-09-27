import { createContext, useContext } from "react";

/** v1 polish F21: the project root (`hello.projectDir`, already canonical in Rust) for the rows and
 *  cards that show a path. `""` before `hello` arrives, which leaves every path as it came. */
export const ProjectDirContext = createContext("");

/** A path under the project root, relative to it; any other path unchanged. What a reader needs is
 *  which file in this project, the way `git status` and Claude Code's own tool lines print one --
 *  the absolute prefix is the same on every row. `.` for the root itself. Only a whole leading
 *  directory counts (`/p/proj` is not a prefix of `/p/project/a`). The click/`gf` target keeps the
 *  path as sent (`data-path`); this is only what is drawn. */
export function projectRelative(path: string, projectDir: string): string {
  const root = projectDir.replace(/\/+$/, "");
  if (root === "" || !path.startsWith("/")) return path;
  if (path === root) return ".";
  return path.startsWith(`${root}/`) ? path.slice(root.length + 1) : path;
}

/** `projectRelative` against the panel's own project root. */
export function useProjectRelative(path: string): string {
  return projectRelative(path, useContext(ProjectDirContext));
}
