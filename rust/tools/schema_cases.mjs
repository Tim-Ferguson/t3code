// Walks an Effect Schema AST for decode test inputs: the cases the parity generators run through
// the original's own decoding (`generate_mcp_tool_parameters.mjs`, `generate_rpc_decode_cases.mjs`).
//
// The cases are walked from the same AST the port interprets, so every field is reached: at each slot
// of the input (property, element, record entry, union member) a plain value the schema holds is
// replaced in turn by wrong types, null, blanks, values around each bound a check states (counted
// in UTF-16 units), a violation of each pattern, a missing required key and an unknown key; every
// union member is tried and malformed; and where a hand-written refinement reads several fields,
// every combination of the optional ones that it can tell apart.
import * as Schema from "../../apps/server/node_modules/effect/dist/Schema.js";

const encodedSide = (ast) =>
  ast.encoding ? encodedSide(ast.encoding[ast.encoding.length - 1].to) : ast;
const decodes = (ast, value) => {
  try {
    Schema.decodeUnknownSync(Schema.make(ast))(value);
    return true;
  } catch {
    return false;
  }
};
const checksOf = (ast) =>
  (ast.checks ?? []).flatMap((check) => (check._tag === "FilterGroup" ? check.checks : [check]));
const representations = (ast) => checksOf(ast).map((check) => check.annotations?.representation);
const stringPool = [
  "x",
  "main",
  "09:30",
  "image/png",
  "a-b",
  "owner/repo",
  "/tmp/a",
  "https://example.com/a",
  "feature-x",
  "a_b",
  "0",
  "pending-00000000-0000-4000-8000-000000000000-txt",
];
const numberPool = [1, 60000, 1000, 100, 0, 10, 5, 3, 1440];

// A plain input the node accepts, or undefined when none is found.
const sample = (ast) => {
  const side = encodedSide(ast);
  const tries = [];
  switch (side._tag) {
    case "String":
    case "TemplateLiteral":
      tries.push(...stringPool, "cmd+k", "mod+k");
      break;
    case "Number":
      tries.push(...numberPool);
      break;
    case "Boolean":
      tries.push(true);
      break;
    case "Null":
      tries.push(null);
      break;
    case "Literal":
      tries.push(side.literal);
      break;
    case "Unknown":
      tries.push("x");
      break;
    case "Union":
      for (const member of side.types) {
        const value = sample(member);
        if (value !== undefined) tries.push(value);
      }
      break;
    case "Objects": {
      const out = {};
      for (const property of side.propertySignatures) {
        if (property.type.context?.isOptional) continue;
        const value = sample(property.type);
        if (value === undefined && !decodes(property.type, undefined)) return undefined;
        if (value !== undefined) out[property.name] = value;
      }
      tries.push(out);
      // A required pair may only hold with an optional one (a choice of two fields, a legacy alias).
      const optional = side.propertySignatures.filter(
        (property) => property.type.context?.isOptional,
      );
      const present = (names) => {
        const next = { ...out };
        for (const name of names) {
          const value = sample(
            side.propertySignatures.find((property) => property.name === name).type,
          );
          if (value !== undefined) next[name] = value;
        }
        return next;
      };
      for (const property of optional) tries.push(present([property.name]));
      if (optional.length) tries.push(present(optional.map((property) => property.name)));
      break;
    }
    case "Arrays":
      tries.push(side.rest.length ? [sample(side.rest[0])].filter((x) => x !== undefined) : []);
      break;
    default:
      return undefined;
  }
  return tries.find((candidate) => decodes(ast, candidate));
};

const wrong = [
  null,
  true,
  false,
  0,
  1,
  -1,
  1.5,
  9007199254740992,
  "",
  "  ",
  " x ",
  [],
  {},
  ["x"],
  [1],
  { unexpected: 1 },
  "x".repeat(300),
];
const bounds = (ast) => {
  const values = [];
  for (const check of checksOf(ast)) {
    const r = check.annotations?.representation;
    if (!r) continue;
    const p = r.payload;
    switch (r.id) {
      case "effect/schema/isMaxLength":
        values.push(
          "x".repeat(p.maxLength),
          "x".repeat(p.maxLength + 1),
          "😀".repeat(Math.floor(p.maxLength / 2)),
          "😀".repeat(Math.floor(p.maxLength / 2) + 1),
          " " + "x".repeat(p.maxLength) + " ",
          Array(p.maxLength).fill("idle"),
          Array(p.maxLength + 1).fill("idle"),
        );
        break;
      case "effect/schema/isMinLength":
        values.push(
          "x".repeat(Math.max(0, p.minLength - 1)),
          "x".repeat(p.minLength),
          Array(Math.max(0, p.minLength - 1)).fill("idle"),
          Array(p.minLength).fill("idle"),
        );
        break;
      case "effect/schema/isPattern":
        values.push("!!bad value!!", "a b", "ABC", "25:00", "9:5", "3/4");
        break;
      case "effect/schema/isGreaterThan":
        values.push(p.exclusiveMinimum, p.exclusiveMinimum + 1, p.exclusiveMinimum + 0.5);
        break;
      case "effect/schema/isGreaterThanOrEqualTo":
        values.push(p.minimum - 1, p.minimum, p.minimum + 1);
        break;
      case "effect/schema/isLessThanOrEqualTo":
        values.push(p.maximum - 1, p.maximum, p.maximum + 1);
        break;
      case "effect/schema/isBetween":
        values.push(p.minimum - 1, p.minimum, p.maximum, p.maximum + 1);
        break;
    }
  }
  return values;
};

const cases = [];
const add = (value) => cases.push(value);
const subsets = (items, cap) => {
  const out = [[]];
  for (const item of items.slice(0, cap))
    for (const existing of [...out]) out.push([...existing, item]);
  return out;
};

// Visits every slot below `ast`. `rebuild(value)` is the whole tool input with `value` in the slot.
const visit = (ast, rebuild, depth) => {
  const side = encodedSide(ast);
  const good = sample(ast);
  // Replacements for this slot.
  const replacements = [...wrong, ...bounds(ast), ...bounds(side)];
  if (side._tag === "Literal") replacements.push("not-the-literal", side.literal === 1 ? 2 : 1);
  if (side._tag === "Union")
    for (const member of side.types) {
      const value = sample(member);
      if (value !== undefined) replacements.push(value);
    }
  if (good !== undefined) replacements.push(good);
  for (const value of replacements) add(rebuild(value));
  // Object slots: each key missing, null, an unknown key, and (record) entries.
  const objectSides = side._tag === "Objects" ? [side] : [];
  for (const object of objectSides) {
    const base = good && typeof good === "object" && !Array.isArray(good) ? good : {};
    add(rebuild({ ...base, unknown: true }));
    // Each optional field on its own beside the required ones, and all of them.
    {
      const required = Object.fromEntries(
        object.propertySignatures
          .filter(
            (property) => !property.type.context?.isOptional && base[property.name] !== undefined,
          )
          .map((property) => [property.name, base[property.name]]),
      );
      const optional = object.propertySignatures.filter(
        (property) => property.type.context?.isOptional,
      );
      for (const property of optional) {
        const value = sample(property.type);
        if (value !== undefined) add(rebuild({ ...required, [property.name]: value }));
      }
      add(
        rebuild(
          Object.fromEntries([
            ...Object.entries(required),
            ...optional
              .map((property) => [property.name, sample(property.type)])
              .filter(([, value]) => value !== undefined),
          ]),
        ),
      );
    }
    for (const key of Object.keys(base)) {
      const { [key]: _gone, ...rest } = base;
      add(rebuild(rest));
    }
    for (const property of object.propertySignatures) {
      const slot = (value) => rebuild({ ...base, [property.name]: value });
      if (depth < 6) visit(property.type, slot, depth + 1);
      // Through a transformation the property's checks bind what the transformation produced.
      if (ast.encoding) {
        const typeSide =
          ast.propertySignatures?.find((candidate) => candidate.name === property.name) ??
          ast.propertySignatures?.find(
            (candidate) => candidate.name === "instanceId" && property.name === "provider",
          );
        if (typeSide)
          for (const value of [...wrong, ...bounds(typeSide.type), sample(typeSide.type)].filter(
            (x) => x !== undefined,
          ))
            add(slot(value));
      }
    }
    for (const signature of object.indexSignatures) {
      const entry = sample(signature.type);
      for (const [key, value] of [
        ["effort", "max"],
        ["", "x"],
        [" k ", " v "],
        ["1", "a"],
        ["a", 1],
        ["a", null],
        ["a", true],
        ["a", " "],
        ["a", { nested: 1 }],
        ["a", [1]],
        ["b", "x"],
        ["10", "x"],
        ["2", false],
      ])
        add(rebuild({ ...base, [key]: value }));
      add(rebuild({ ...base, a: "x", b: true, c: " y ", d: 3 }));
      if (entry !== undefined && depth < 6)
        visit(signature.type, (value) => rebuild({ ...base, a: value }), depth + 1);
    }
    // A hand-written refinement reads several fields: try the combinations of the optional ones.
    const refined = checksOf(ast).some(
      (check) => !check.annotations?.representation && check.annotations?.expected === undefined,
    );
    if (refined) {
      const optional = object.propertySignatures.filter(
        (property) => property.type.context?.isOptional,
      );
      const literals = object.propertySignatures.filter(
        (property) =>
          encodedSide(property.type)._tag === "Union" &&
          encodedSide(property.type).types.every((member) => member._tag === "Literal"),
      );
      const modes = literals.length ? literals[0].type : undefined;
      const alternatives = modes
        ? encodedSide(modes).types.map((member) => member.literal)
        : [undefined];
      for (const mode of alternatives)
        for (const chosen of subsets(
          optional.map((property) => property.name).filter((name) => name !== literals[0]?.name),
          8,
        )) {
          const input = { ...base };
          if (mode !== undefined) input[literals[0].name] = mode;
          for (const name of chosen) {
            const value = sample(
              object.propertySignatures.find((property) => property.name === name).type,
            );
            if (value !== undefined) input[name] = value;
          }
          add(rebuild(input));
          if (input.width !== undefined) add(rebuild({ ...input, width: 3840, height: 2161 }));
        }
    }
  }
  // Array slots: lengths, each element, unknown content.
  if (side._tag === "Arrays" && side.rest.length) {
    const item = sample(side.rest[0]);
    if (item !== undefined) {
      add(rebuild([item, item]));
      add(rebuild([item, null]));
      add(rebuild([null, item]));
      add(rebuild(Array(11).fill(item)));
    }
    if (depth < 6) visit(side.rest[0], (value) => rebuild([value]), depth + 1);
  }
  // Union slots: every member on its own.
  if (side._tag === "Union" && depth < 6)
    for (const member of side.types) visit(member, rebuild, depth + 1);
};

export const casesFor = (ast) => {
  cases.length = 0;
  for (const value of [{}, null, [], false, 0, "", "x", { unknown: true }]) add(value);
  const good = sample(ast) ?? {};
  add(good);
  add({ ...good, unknown: true });
  visit(ast, (value) => value, 0);
  const seen = new Set();
  return cases.filter((input) => {
    const key = JSON.stringify(input) ?? "undefined";
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
};
