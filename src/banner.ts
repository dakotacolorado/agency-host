/** agency-host's banner: its name in block letters beside agency's mark, then agency's tagline, as `--help` and `--version` and the README open. */

/** agency's letters, then `-host`, in the same face. */
const LETTERS = [
  " █████╗  ██████╗ ███████╗███╗   ██╗ ██████╗██╗   ██╗      ██╗  ██╗ ██████╗ ███████╗████████╗",
  "██╔══██╗██╔════╝ ██╔════╝████╗  ██║██╔════╝╚██╗ ██╔╝      ██║  ██║██╔═══██╗██╔════╝╚══██╔══╝",
  "███████║██║  ███╗█████╗  ██╔██╗ ██║██║      ╚████╔╝ █████╗███████║██║   ██║███████╗   ██║",
  "██╔══██║██║   ██║██╔══╝  ██║╚██╗██║██║       ╚██╔╝  ╚════╝██╔══██║██║   ██║╚════██║   ██║",
  "██║  ██║╚██████╔╝███████╗██║ ╚████║╚██████╗   ██║         ██║  ██║╚██████╔╝███████║   ██║",
  "╚═╝  ╚═╝ ╚═════╝ ╚══════╝╚═╝  ╚═══╝ ╚═════╝   ╚═╝         ╚═╝  ╚═╝ ╚═════╝ ╚══════╝   ╚═╝",
];

/**
 * agency's mark (`MARK_PIXELS` in agency's `src/banner.ts`) on its 18 by 12
 * grid: the bureau, a pediment over four columns, each column an agent
 * holding it up. A terminal draws it two rows a character, as many rows as the letters.
 */
export const MARK_PIXELS = [
  "........##........",
  "......######......",
  "....##########....",
  "..##############..",
  ".################.",
  "..................",
  "..##..##..##..##..",
  "..##..##..##..##..",
  "..##..##..##..##..",
  "..##..##..##..##..",
  ".################.",
  "##################",
];

/** agency's line, verbatim. */
export const TAGLINE = "Agents at your command, across all your computers.";
/** What agency-host is, in one line. */
export const SUMMARY = "agency-host runs agency's agents on this computer, each a Claude Code session on the Claude Agent SDK.";

const MARK = Array.from({ length: MARK_PIXELS.length / 2 }, (_, row) =>
  [...MARK_PIXELS[row * 2]!]
    .map((top, col) => {
      const t = top === "#", b = MARK_PIXELS[row * 2 + 1]![col] === "#";
      return t && b ? "█" : t ? "▀" : b ? "▄" : " ";
    })
    .join("")
    .trimEnd(),
);

const WIDTH = Math.max(...LETTERS.map((line) => line.length));
export const WORDMARK = LETTERS.map((line, i) => `${line.padEnd(WIDTH)}  ${MARK[i]}`.trimEnd()).join("\n");

/** Whether to paint: a terminal that has not asked for none (https://no-color.org). */
export function wantsColor(stream: { isTTY?: boolean }, env: NodeJS.ProcessEnv = process.env): boolean {
  return Boolean(stream.isTTY) && !env.NO_COLOR;
}

/** The banner, its mark and letters in agency's brass when `color` is on, then the tagline and what this is. */
export function banner(version: string, color: boolean): string {
  const paint = (code: string, text: string) => (color ? `\x1b[${code}m${text}\x1b[0m` : text);
  return [paint("38;5;178", WORDMARK), paint("2", `v${version}`), "", paint("1", TAGLINE), SUMMARY].join("\n");
}
