// Stands in for a code generator such as an ORM client: it reads a schema
// and writes generated code inside node_modules, next to a copy of the
// schema it was generated from. After a clone, .fast-worktree.toml skips
// it when that copy is byte-identical to the schema.
const fs = require("fs");
const schema = fs.readFileSync("schema.json", "utf8");
fs.mkdirSync("node_modules/.codegen", { recursive: true });
fs.writeFileSync("node_modules/.codegen/schema.json", schema);
const fields = Object.keys(JSON.parse(schema).fields);
fs.writeFileSync(
  "node_modules/.codegen/index.js",
  `module.exports = ${JSON.stringify(fields)};\n`,
);
