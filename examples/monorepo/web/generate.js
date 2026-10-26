// Stands in for a project's own postinstall that writes outside
// node_modules (code generation, git hooks setup). fast-worktree runs it
// again after cloning node_modules, so the generated file is always there.
const fs = require("fs");
const ms = require("ms");
fs.writeFileSync("generated.txt", `one day is ${ms("1d")} ms\n`);
