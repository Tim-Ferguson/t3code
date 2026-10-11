// Where everything lives under the scratch directory, and the checks that keep it there.
import { existsSync, readdirSync, realpathSync } from "node:fs";
import { homedir } from "node:os";
import { basename, dirname, isAbsolute, join, resolve, sep } from "node:path";

export const NAME_PREFIX = "t3code-rust-port-test-";

export function layout(scratch) {
  return {
    scratch,
    state: join(scratch, "state"),
    home: join(scratch, "home"),
    bin: join(scratch, "bin"),
    tmp: join(scratch, "tmp"),
    logs: join(scratch, "logs"),
    providers: join(scratch, "providers"),
    remotes: join(scratch, "remotes"),
    // A file here makes one fake write fail (README, "Toggles").
    toggles: join(scratch, "toggles"),
    // The server serves the directory it runs in: the diff panel refuses a repository outside it, so it runs in the
    // directory that holds every project.
    serverCwd: join(scratch, "projects"),
    settings: join(scratch, "settings.json"),
    sandbox: join(scratch, "server.sb"),
    pid: join(scratch, "server.pid"),
    log: join(scratch, "server.log"),
    meta: join(scratch, "fixture.json"),
    calls: join(scratch, "logs", "fake-calls.jsonl"),
    projects: {
      dirty: join(scratch, "projects", NAME_PREFIX + "dirty"),
      github: join(scratch, "projects", NAME_PREFIX + "github"),
      gitlab: join(scratch, "projects", NAME_PREFIX + "gitlab"),
      conversations: join(scratch, "projects", NAME_PREFIX + "conversations"),
      commitcheck: join(scratch, "projects", NAME_PREFIX + "commitcheck"),
      ship: join(scratch, "projects", NAME_PREFIX + "ship"),
    },
  };
}

/** Throws unless `scratch` is a safe place to put a throwaway server. */
export function validateScratch(input, { reuse }) {
  if (!input || !isAbsolute(input)) throw new Error("--scratch must be an absolute path");
  const scratch = resolve(input);
  const real = (path) => {
    let probe = path;
    while (!existsSync(probe) && dirname(probe) !== probe) probe = dirname(probe);
    return join(realpathSync(probe), path.slice(probe.length));
  };
  const target = real(scratch);
  const home = realpathSync(homedir());
  const under = (child, parent) => child === parent || child.startsWith(parent + sep);
  if (target === sep || target === home || under(home, target))
    throw new Error(`refusing ${scratch}: it is, or contains, the home directory`);
  if (under(target, join(home, ".t3"))) throw new Error(`refusing ${scratch}: it is under ~/.t3`);
  if (under(target, home)) {
    const first = target.slice(home.length + 1).split(sep)[0];
    if (first.startsWith("."))
      throw new Error(
        `refusing ${scratch}: it is under the dotfiles of the real home directory (${join(home, first)})`,
      );
  }
  if (!basename(target).startsWith(NAME_PREFIX))
    throw new Error(
      `refusing ${scratch}: the directory name must start with ${NAME_PREFIX} so that anyone who finds it knows what it is`,
    );
  if (existsSync(target) && readdirSync(target).length > 0 && !reuse) {
    throw new Error(
      `refusing ${scratch}: it is not empty (pass --reuse to start again on top of an earlier run's directory)`,
    );
  }
  return target;
}
