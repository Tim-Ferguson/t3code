// Writes crates/server/src/text_generation_prompts.json from the original text generation sources, so
// the Rust port sends the words the original sends: the prompts, the structured output schemas, the
// writing-style presets and the numbers that bound them. Nothing here is typed by hand; each prompt
// is built by the unchanged TypeScript builder with sentinel inputs and cut at the sentinels into
// templates with `{{slot}}` holes. Run: node tools/generate_text_generation_prompts.mjs
import { writeFileSync, renameSync } from "node:fs";
import * as Prompts from "../../apps/server/src/textGeneration/TextGenerationPrompts.ts";
import * as Utils from "../../apps/server/src/textGeneration/TextGenerationUtils.ts";
import * as Presets from "../../apps/server/src/textGeneration/TextGenerationPresets.ts";
import * as Model from "../../packages/contracts/src/model.ts";

const slot = (text, pairs) => {
  let out = text;
  for (const [sentinel, name] of pairs) {
    if (!out.includes(sentinel)) throw new Error(`sentinel ${name} missing`);
    out = out.split(sentinel).join(`{{${name}}}`);
  }
  if (/@@/.test(out)) throw new Error("unreplaced sentinel");
  return out;
};
const schema = (built) => Utils.toJsonSchemaObject(built.outputSchema);

// ---- commit message: with and without the branch key, with and without style instructions ----
const commit = {};
for (const branch of [false, true]) {
  for (const policy of [false, true]) {
    const built = Prompts.buildCommitMessagePrompt({
      branch: "@@BRANCH@@",
      stagedSummary: "@@SUMMARY@@",
      stagedPatch: "@@PATCH@@",
      includeBranch: branch,
      ...(policy
        ? {
            policy: {
              kind: "custom",
              inferRepositoryConventions: false,
              commitInstructions: "@@POLICY@@",
            },
          }
        : {}),
    });
    commit[`${branch ? "branch" : "plain"}${policy ? "+policy" : ""}`] = {
      template: slot(built.prompt, [
        ["@@BRANCH@@", "branch"],
        ["@@SUMMARY@@", "staged_summary"],
        ["@@PATCH@@", "staged_patch"],
        ...(policy ? [["@@POLICY@@", "policy"]] : []),
      ]),
      schema: schema(built),
    };
  }
}

// ---- change request content: with and without the repository template and style instructions ----
const changeRequest = {};
for (const template of [false, true]) {
  for (const policy of [false, true]) {
    const built = Prompts.buildPrContentPrompt({
      baseBranch: "@@BASE@@",
      headBranch: "@@HEAD@@",
      commitSummary: "@@COMMITS@@",
      diffSummary: "@@STAT@@",
      diffPatch: "@@PATCH@@",
      ...(template ? { changeRequestTemplate: "@@TEMPLATE@@" } : {}),
      ...(policy
        ? {
            policy: {
              kind: "custom",
              inferRepositoryConventions: false,
              changeRequestInstructions: "@@POLICY@@",
            },
          }
        : {}),
    });
    changeRequest[`${template ? "template" : "plain"}${policy ? "+policy" : ""}`] = {
      template: slot(built.prompt, [
        ["@@BASE@@", "base_branch"],
        ["@@HEAD@@", "head_branch"],
        ["@@COMMITS@@", "commit_summary"],
        ["@@STAT@@", "diff_summary"],
        ["@@PATCH@@", "diff_patch"],
        ...(template ? [["@@TEMPLATE@@", "template"]] : []),
        ...(policy ? [["@@POLICY@@", "policy"]] : []),
      ]),
      schema: schema(built),
    };
  }
}

// ---- branch name: by naming mode, with and without custom instructions and attachments ----
const attachment = {
  type: "image",
  id: "@@ID@@",
  name: "@@ATTACHMENT@@",
  mimeType: "image/png",
  sizeBytes: 1,
};
const branch = {};
for (const mode of ["static", "semantic", "custom"]) {
  for (const instructions of mode === "custom" ? [false, true] : [false]) {
    for (const attachments of [false, true]) {
      const built = Prompts.buildBranchNamePrompt({
        message: "@@MESSAGE@@",
        naming: { mode, prefix: "t3", instructions: instructions ? "@@INSTRUCTIONS@@" : "" },
        ...(attachments ? { attachments: [attachment] } : {}),
      });
      const text = attachments
        ? built.prompt.replace("- @@ATTACHMENT@@ (image/png, 1 bytes)", "@@LINES@@")
        : built.prompt;
      branch[`${mode}${instructions ? "+instructions" : ""}${attachments ? "+attachments" : ""}`] =
        {
          template: slot(text, [
            ["@@MESSAGE@@", "message"],
            ...(instructions ? [["@@INSTRUCTIONS@@", "instructions"]] : []),
            ...(attachments ? [["@@LINES@@", "attachments"]] : []),
          ]),
          schema: schema(built),
        };
    }
  }
}

// ---- thread title: the two heads and the pieces appended after the message. The original never
// passes a style policy to a title, so none is carried. ----
const title = (input) => Prompts.buildThreadTitlePrompt(input);
const initial = title({ message: "@@MESSAGE@@" });
const regenerate = title({ message: "@@MESSAGE@@", previousTitle: "@@PREVIOUS@@" });
const initialLabel = "\n\nUser message:\n@@MESSAGE@@";
const regenerateLabel = "\n\nThread contents:\n@@MESSAGE@@";
if (!initial.prompt.endsWith(initialLabel) || !regenerate.prompt.endsWith(regenerateLabel))
  throw new Error("title prompt layout changed");
const linked = title({ message: "@@MESSAGE@@", linkedContext: "@@LINKED@@" }).prompt.slice(
  initial.prompt.length,
);
const withAttachments = title({ message: "@@MESSAGE@@", attachments: [attachment] }).prompt.slice(
  initial.prompt.length,
);
const threadTitle = {
  initialHead: initial.prompt.slice(0, -initialLabel.length),
  initialLabel: "\n\nUser message:\n",
  regenerateHead: slot(regenerate.prompt.slice(0, -regenerateLabel.length), [
    [JSON.stringify("@@PREVIOUS@@"), "previous_title_json"],
  ]),
  regenerateLabel: "\n\nThread contents:\n",
  linked: slot(linked, [["@@LINKED@@", "linked"]]),
  attachments: slot(withAttachments.replace("- @@ATTACHMENT@@ (image/png, 1 bytes)", "@@LINES@@"), [
    ["@@LINES@@", "attachments"],
  ]),
  schema: schema(initial),
};

const data = {
  commit,
  changeRequest,
  branch,
  threadTitle,
  presets: {
    conventional_commits: {
      commitInstructions: Presets.conventionalCommitsTextGenerationPolicy.commitInstructions,
      changeRequestInstructions:
        Presets.conventionalCommitsTextGenerationPolicy.changeRequestInstructions,
    },
    repo_conventions: {
      commitInstructions: Presets.repositoryConventionsTextGenerationPolicy.commitInstructions,
      changeRequestInstructions:
        Presets.repositoryConventionsTextGenerationPolicy.changeRequestInstructions,
    },
  },
  defaults: {
    textGenerationModel: Model.DEFAULT_TEXT_GENERATION_MODEL,
    reasoningEffort: Model.DEFAULT_TEXT_GENERATION_REASONING_EFFORT,
    textGenerationModelByProvider: {
      codex: Model.DEFAULT_TEXT_GENERATION_MODEL_BY_PROVIDER.codex,
      claudeAgent: Model.DEFAULT_TEXT_GENERATION_MODEL_BY_PROVIDER.claudeAgent,
    },
    modelByProvider: {
      codex: Model.DEFAULT_MODEL_BY_PROVIDER.codex,
      claudeAgent: Model.DEFAULT_MODEL_BY_PROVIDER.claudeAgent,
    },
  },
};
const out = `${JSON.stringify(data, null, 1)}\n`;
const target = new URL("../crates/server/src/text_generation_prompts.json", import.meta.url);
const temporary = new URL(`${target.href}.tmp`);
writeFileSync(temporary, out);
renameSync(temporary, target);
console.log(
  JSON.stringify({
    bytes: out.length,
    commit: Object.keys(commit).length,
    changeRequest: Object.keys(changeRequest).length,
    branch: Object.keys(branch).length,
  }),
);
