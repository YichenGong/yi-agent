---
name: skill-installer
description: Guide for installing skills from external sources like GitHub repositories. Use when users want to install, download, or share skills from outside the local filesystem.
---

# Skill Installer

Helps install skills from external sources. A skill is just a directory containing a SKILL.md file; installation is simply copying that directory into a skill root.

## Skill Roots

yi-agent discovers skills from three locations:

1. **Project**: `<project>/.yi-agent/skills/` -- only available in that project
2. **User**: `~/.yi-agent/skills/` -- available across all projects
3. **System**: `~/.yi-agent/skills/.system/` -- bundled skills shipped with yi-agent (do not modify)

Install new skills into the User or Project root. Use the User root for skills you want everywhere, and the Project root for skills specific to one project.

## Installation Methods

### From a GitHub Repository

Clone the repository into a skill root. For a single skill:

```bash
git clone https://github.com/<owner>/<repo>.git ~/.yi-agent/skills/<skill-name>
```

Or, if the repo contains multiple skills in a subdirectory, use sparse-checkout or copy the specific skill directory:

```bash
# Clone to a temp location
git clone --depth 1 https://github.com/<owner>/<repo>.git /tmp/skills-repo
# Copy the specific skill
cp -r /tmp/skills-repo/<skill-name> ~/.yi-agent/skills/<skill-name>
# Clean up
rm -rf /tmp/skills-repo
```

### From a Local Directory

If the skill is already on the local filesystem, copy it:

```bash
cp -r /path/to/skill-name ~/.yi-agent/skills/<skill-name>
```

### From a Zip/Tar Archive

Extract the archive into a skill root:

```bash
# tar
tar -xzf /path/to/skill.tar.gz -C ~/.yi-agent/skills/
# zip
unzip /path/to/skill.zip -d ~/.yi-agent/skills/
```

## Validation After Installation

After installing a skill, verify it is valid:

1. Check that `<installed-path>/SKILL.md` exists
2. Check that the SKILL.md has YAML frontmatter with `name` and `description`:
   ```bash
   head -5 ~/.yi-agent/skills/<skill-name>/SKILL.md
   ```
3. Check that the `name` field matches `^[a-z0-9]+(-[a-z0-9]+)*$` and is <= 64 chars
4. Check that the `description` field is present and <= 1024 chars

If any check fails, remove the skill directory and report the issue.

## Restart to Load

yi-agent discovers skills at startup. After installing a new skill, restart yi-agent to load it. There is no hot-reload.

## Communication

When listing installed skills, use a command like:

```bash
ls ~/.yi-agent/skills/ <project>/.yi-agent/skills/ 2>/dev/null
```

After installing a skill, tell the user:
- The skill is installed at `<path>`
- It will be available on the next yi-agent restart
- They can trigger it by asking yi-agent to do something related to the skill's description

## Behavior and Options

- Aborts installation if the destination skill directory already exists (user must remove it first to reinstall)
- Installs into `~/.yi-agent/skills/<skill-name>` by default for user-level skills
- For project-level skills, install into `<project>/.yi-agent/skills/<skill-name>`
- Multiple skills can be installed in one session by repeating the clone/copy commands

## Notes

- The skills at `~/.yi-agent/skills/.system/` are preinstalled by yi-agent itself. If users ask about them, explain they are bundled and auto-updated on yi-agent updates. Do not modify them directly.
- Private GitHub repos can be accessed via existing git credentials (SSH keys, credential helper, or `GITHUB_TOKEN` env var).
- If a user provides a GitHub URL pointing to a specific subdirectory, use sparse-checkout or the copy-from-temp pattern shown above.
