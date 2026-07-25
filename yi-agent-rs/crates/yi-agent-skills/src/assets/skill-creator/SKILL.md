---
name: skill-creator
description: Guide for creating effective skills. Use when users want to create a new skill or update an existing one. Covers naming, directory structure, SKILL.md format, and the references/scripts/assets subdirectories.
---

# Skill Creator

This skill provides guidance for creating effective skills for yi-agent.

## About Skills

Skills are modular, self-contained folders that extend yi-agent's capabilities by providing specialized knowledge, workflows, and tools. Think of them as "onboarding guides" for specific domains or tasks -- they transform yi-agent from a general-purpose agent into a specialized agent equipped with procedural knowledge that no model can fully possess.

### What Skills Provide

1. Specialized workflows - Multi-step procedures for specific domains
2. Tool integrations - Instructions for working with specific file formats or APIs
3. Domain expertise - Company-specific knowledge, schemas, business logic
4. Bundled resources - Scripts, references, and assets for complex and repetitive tasks

## Core Principles

### Concise is Key

The context window is a shared resource. Skills share the context window with everything else yi-agent needs: system prompt, conversation history, other skills' metadata, and the actual user request.

**Default assumption: yi-agent is already very smart.** Only add context yi-agent doesn't already have. Challenge each piece of information: "Does yi-agent really need this explanation?" and "Does this paragraph justify its token cost?"

Prefer concise examples over verbose explanations.

### Set Appropriate Degrees of Freedom

Match the level of specificity to the task's fragility and variability:

**High freedom (text-based instructions)**: Use when multiple approaches are valid, decisions depend on context, or heuristics guide the approach.

**Medium freedom (pseudocode or scripts with parameters)**: Use when a preferred pattern exists, some variation is acceptable, or configuration affects behavior.

**Low freedom (specific scripts, few parameters)**: Use when operations are fragile and error-prone, consistency is critical, or a specific sequence must be followed.

### Anatomy of a Skill

Every skill consists of a required SKILL.md file and optional bundled resources:

```
skill-name/
├── SKILL.md (required)
│   ├── YAML frontmatter (required)
│   │   ├── name: (required)
│   │   └── description: (required)
│   └── Markdown instructions (required)
└── Bundled Resources (optional)
    ├── scripts/          - Executable code (Python/Bash/etc.)
    ├── references/       - Documentation loaded into context as needed
    └── assets/           - Files used in output (templates, icons, fonts, etc.)
```

#### SKILL.md (required)

Every SKILL.md consists of:

- **Frontmatter** (YAML): Contains `name` and `description` fields. These are the only fields yi-agent reads to determine when the skill gets used, thus it is very important to be clear and comprehensive in describing what the skill is, and when it should be used.
- **Body** (Markdown): Instructions and guidance for using the skill. Only loaded AFTER the skill triggers (when the LLM calls the Skill tool).

#### Bundled Resources (optional)

##### Scripts (`scripts/`)

Executable code (Python/Bash/etc.) for tasks that require deterministic reliability or are repeatedly rewritten.

- **When to include**: When the same code is being rewritten repeatedly or deterministic reliability is needed
- **Example**: `scripts/rotate_pdf.py` for PDF rotation tasks
- **Benefits**: Token efficient, deterministic, may be executed without loading into context
- **Usage**: yi-agent runs these via the existing `bash` tool

##### References (`references/`)

Documentation and reference material intended to be loaded as needed into context to inform yi-agent's process and thinking.

- **When to include**: For documentation that yi-agent should reference while working
- **Examples**: `references/finance.md` for financial schemas, `references/api_docs.md` for API specifications
- **Use cases**: Database schemas, API documentation, domain knowledge, company policies
- **Benefits**: Keeps SKILL.md lean, loaded only when yi-agent determines it's needed
- **Best practice**: If files are large, include grep search patterns in SKILL.md
- **Avoid duplication**: Information should live in either SKILL.md or references files, not both

##### Assets (`assets/`)

Files not intended to be loaded into context, but rather used within the output yi-agent produces.

- **When to include**: When the skill needs files that will be used in the final output
- **Examples**: `assets/logo.png` for brand assets, `assets/template.html` for HTML boilerplate
- **Use cases**: Templates, images, icons, boilerplate code, sample documents

#### What Not to Include in a Skill

A skill should only contain essential files that directly support its functionality. Do NOT create extraneous documentation or auxiliary files, including:

- README.md
- INSTALLATION_GUIDE.md
- QUICK_REFERENCE.md
- CHANGELOG.md

The skill should only contain the information needed for an AI agent to do the job at hand.

### Progressive Disclosure Design Principle

Skills use a three-level loading system to manage context efficiently:

1. **Metadata (name + description)** - Always in context (in the system prompt catalog, ~100 words per skill)
2. **SKILL.md body** - When the LLM calls the Skill tool (<5k words recommended)
3. **Bundled resources** - As needed by yi-agent (unlimited because scripts can be executed without reading into context)

#### Progressive Disclosure Patterns

Keep SKILL.md body to the essentials and under 500 lines to minimize context bloat. Split content into separate files when approaching this limit. When splitting content into other files, reference them from SKILL.md and describe clearly when to read them.

**Key principle:** When a skill supports multiple variations, frameworks, or options, keep only the core workflow and selection guidance in SKILL.md. Move variant-specific details (patterns, examples, configuration) into separate reference files.

**Pattern 1: High-level guide with references**

```markdown
# PDF Processing

## Quick start

Extract text with pdfplumber:
[code example]

## Advanced features

- **Form filling**: See [FORMS.md](FORMS.md) for complete guide
- **API reference**: See [REFERENCE.md](REFERENCE.md) for all methods
```

yi-agent loads FORMS.md or REFERENCE.md only when needed (via the `read` tool).

**Pattern 2: Domain-specific organization**

For skills with multiple domains, organize content by domain to avoid loading irrelevant context:

```
bigquery-skill/
├── SKILL.md (overview and navigation)
└── references/
    ├── finance.md (revenue, billing metrics)
    ├── sales.md (opportunities, pipeline)
    └── product.md (API usage, features)
```

When the user asks about sales metrics, yi-agent only reads sales.md.

## Skill Creation Process

### Skill Naming

- Use lowercase letters, digits, and hyphens only: `^[a-z0-9]+(-[a-z0-9]+)*$`
- Maximum 64 characters
- Prefer short, verb-led phrases that describe the action
- Namespace by tool when it improves clarity (e.g., `gh-address-comments`, `linear-address-issue`)
- Name the skill folder exactly after the skill name

### Step 1: Understand the Skill with Concrete Examples

To create an effective skill, clearly understand concrete examples of how the skill will be used. This understanding can come from either direct user examples or generated examples that are validated with user feedback.

For example, when building an image-editor skill, relevant questions include:

- "What functionality should the image-editor skill support?"
- "Can you give me some examples of how this skill would be used?"
- "What would a user say that should trigger this skill?"
- "Where should I create this skill? Default is `~/.yi-agent/skills/` for user-wide, or `<project>/.yi-agent/skills/` for project-only."

To avoid overwhelming users, avoid asking too many questions in a single message.

### Step 2: Plan the Reusable Skill Contents

Analyze each concrete example to identify reusable resources:

1. Consider how to execute on the example from scratch
2. Identify what scripts, references, and assets would be helpful when executing these workflows repeatedly

Example: When building a `pdf-editor` skill to handle "Help me rotate this PDF":
1. Rotating a PDF requires re-writing the same code each time
2. A `scripts/rotate_pdf.py` script would be helpful to store in the skill

Example: When building a `big-query` skill for "How many users have logged in today?":
1. Querying BigQuery requires re-discovering the table schemas each time
2. A `references/schema.md` file documenting the table schemas would be helpful

### Step 3: Create the Skill

Create the skill directory and SKILL.md manually. No init script is provided (unlike codex); keep it simple.

```bash
mkdir -p ~/.yi-agent/skills/<skill-name>
# Then create SKILL.md inside
```

### Step 4: Edit the Skill

When editing the skill, remember that the skill is being created for another instance of yi-agent to use. Include information that would be beneficial and non-obvious. Consider what procedural knowledge, domain-specific details, or reusable assets would help another yi-agent execute these tasks more effectively.

#### Start with Reusable Skill Contents

Start with the reusable resources identified above: `scripts/`, `references/`, and `assets/` files. This may require user input (e.g., brand assets, documentation).

Added scripts must be tested by actually running them to ensure there are no bugs. Use the `bash` tool to test.

#### Update SKILL.md

**Writing Guidelines:** Always use imperative/infinitive form.

##### Frontmatter

Write the YAML frontmatter with `name` and `description`:

- `name`: The skill name (lowercase, hyphens, digits, <= 64 chars)
- `description`: This is the primary triggering mechanism for the skill, and helps yi-agent understand when to use the skill.
  - Include both what the Skill does and specific triggers/contexts for when to use it.
  - Include all "when to use" information here -- NOT in the body. The body is only loaded after triggering, so "When to Use This Skill" sections in the body are not helpful to yi-agent.
  - Example description for a `docx` skill: "Comprehensive document creation, editing, and analysis with support for tracked changes, comments, formatting preservation, and text extraction. Use when yi-agent needs to work with professional documents (.docx files) for: (1) Creating new documents, (2) Modifying or editing content, (3) Working with tracked changes, (4) Adding comments, or any other document tasks"

Do not include any other fields in YAML frontmatter.

##### Body

Write instructions for using the skill and its bundled resources.

### Step 5: Validate

After writing, verify:

- SKILL.md has valid YAML frontmatter with `name` and `description`
- The `name` matches `^[a-z0-9]+(-[a-z0-9]+)*$` and is <= 64 chars
- The `description` is <= 1024 chars
- Scripts in `scripts/` are executable and tested
- Reference files in `references/` are referenced from SKILL.md
- No extraneous files (README.md, CHANGELOG.md, etc.)

### Step 6: Iterate

After testing the skill, you may detect issues or users may request improvements.

**Iteration workflow:**

1. Use the skill on real tasks
2. Notice struggles or inefficiencies
3. Identify how SKILL.md or bundled resources should be updated
4. Implement changes and test again

## Placement

Choose the right location for the skill:

- `~/.yi-agent/skills/<skill-name>/` -- user-level, available across all projects
- `<project>/.yi-agent/skills/<skill-name>/` -- project-level, only available in that project

Project skills take precedence in the catalog when both exist with the same name (both are listed, but project appears first).

## Restart to Reload

yi-agent discovers skills at startup. After adding or modifying a skill, restart yi-agent to load the changes. There is no hot-reload.
