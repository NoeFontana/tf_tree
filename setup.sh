#!/usr/bin/env bash
# Bootstrap a new project from rust-python-template.
#
# Prompts for the project name (hyphenated, Rust crate convention), author,
# email, and GitHub owner. Renames the crates, the Python package, and the
# metadata across files. Runs `uv sync`, creates the initial commit, then
# self-deletes.

set -euo pipefail

TEMPLATE_PROJECT_NAME="rust-python-template"
TEMPLATE_PACKAGE_NAME="rust_python_template"
TEMPLATE_AUTHOR_NAME="Noé Fontana"
TEMPLATE_AUTHOR_EMAIL="noe.fontana.pro@gmail.com"
TEMPLATE_GITHUB_OWNER="NoeFontana"

echo "Welcome to the rust-python-template bootstrap script."
echo "Press Enter to keep the default value shown in [brackets]."
echo

read -rp "Project name (hyphen-case, e.g., my-cool-lib) [${TEMPLATE_PROJECT_NAME}]: " project_name
project_name=${project_name:-${TEMPLATE_PROJECT_NAME}}

default_package=$(echo "${project_name}" | tr '-' '_')
read -rp "Python package name (snake_case) [${default_package}]: " project_package
project_package=${project_package:-${default_package}}

read -rp "Author name [${TEMPLATE_AUTHOR_NAME}]: " author_name
author_name=${author_name:-${TEMPLATE_AUTHOR_NAME}}

read -rp "Author email [${TEMPLATE_AUTHOR_EMAIL}]: " author_email
author_email=${author_email:-${TEMPLATE_AUTHOR_EMAIL}}

read -rp "GitHub owner (user or org) [${TEMPLATE_GITHUB_OWNER}]: " github_owner
github_owner=${github_owner:-${TEMPLATE_GITHUB_OWNER}}

cat <<EOF

Bootstrapping with:
  project name:    ${project_name}
  python package:  ${project_package}
  author:          ${author_name} <${author_email}>
  github owner:    ${github_owner}

EOF
read -rp "Proceed? [y/N] " confirm
case "${confirm}" in
    [yY]|[yY][eE][sS]) ;;
    *) echo "Aborted."; exit 1 ;;
esac

# --- Rename crate directories ---
if [[ "${project_name}" != "${TEMPLATE_PROJECT_NAME}" ]]; then
    echo "Renaming crate directories..."
    if [[ -d "crates/${TEMPLATE_PROJECT_NAME}-core" ]]; then
        mv "crates/${TEMPLATE_PROJECT_NAME}-core" "crates/${project_name}-core"
    fi
    if [[ -d "crates/${TEMPLATE_PROJECT_NAME}-ffi" ]]; then
        mv "crates/${TEMPLATE_PROJECT_NAME}-ffi" "crates/${project_name}-ffi"
    fi
fi

# --- Rename Python package directory ---
if [[ "${project_package}" != "${TEMPLATE_PACKAGE_NAME}" && -d "python/${TEMPLATE_PACKAGE_NAME}" ]]; then
    echo "Renaming Python package directory..."
    mv "python/${TEMPLATE_PACKAGE_NAME}" "python/${project_package}"
fi

# --- Replace tokens across project files ---
# Token replacements are applied in dependency order: most specific first, so
# we don't accidentally double-substitute (e.g., the package name appears
# inside the hyphenated project name).
replace_in_files() {
    local search=$1
    local replace=$2
    # Skip .git, target, .venv, node_modules, this script, uv.lock, and the
    # binary lock files. Use grep -l to limit sed to files actually containing
    # the search string, which keeps the run fast and avoids touching binaries.
    local files
    files=$(grep -rlF "${search}" . \
        --exclude-dir=.git \
        --exclude-dir=target \
        --exclude-dir=.venv \
        --exclude-dir=node_modules \
        --exclude-dir=__pycache__ \
        --exclude-dir=dist \
        --exclude=setup.sh \
        --exclude=uv.lock \
        --exclude=Cargo.lock \
        2>/dev/null || true)
    if [[ -z "${files}" ]]; then
        return 0
    fi
    if [[ "${OSTYPE:-}" == "darwin"* ]]; then
        echo "${files}" | xargs sed -i '' "s|${search}|${replace}|g"
    else
        echo "${files}" | xargs sed -i "s|${search}|${replace}|g"
    fi
}

echo "Updating project files..."

if [[ "${project_package}" != "${TEMPLATE_PACKAGE_NAME}" ]]; then
    replace_in_files "${TEMPLATE_PACKAGE_NAME}" "${project_package}"
fi

if [[ "${project_name}" != "${TEMPLATE_PROJECT_NAME}" ]]; then
    replace_in_files "${TEMPLATE_PROJECT_NAME}" "${project_name}"
fi

if [[ "${github_owner}" != "${TEMPLATE_GITHUB_OWNER}" ]]; then
    replace_in_files "${TEMPLATE_GITHUB_OWNER}" "${github_owner}"
fi

if [[ "${author_email}" != "${TEMPLATE_AUTHOR_EMAIL}" ]]; then
    replace_in_files "${TEMPLATE_AUTHOR_EMAIL}" "${author_email}"
fi

if [[ "${author_name}" != "${TEMPLATE_AUTHOR_NAME}" ]]; then
    replace_in_files "${TEMPLATE_AUTHOR_NAME}" "${author_name}"
fi

# --- Sync the venv ---
if command -v uv >/dev/null 2>&1; then
    echo "Running uv sync..."
    uv sync
else
    echo "Warning: 'uv' not found on PATH; skipping 'uv sync'. Install uv from https://docs.astral.sh/uv/ and run 'just bootstrap'."
fi

# --- Create the initial commit ---
if [[ -d ".git" ]]; then
    echo "Creating initial commit..."
    git add -A
    git commit -m "chore: bootstrap from rust-python-template" || \
        echo "Note: nothing to commit, repository already clean."
else
    echo "Warning: not a git repository; skipping initial commit."
fi

# --- Self-delete ---
echo "Removing bootstrap script."
rm -- "$0"

cat <<EOF

Done. Next steps:
  1. Review the diff: git log -1 --stat
  2. Run the test suite: just bootstrap && just test
  3. Push to your remote: git remote add origin git@github.com:${github_owner}/${project_name}.git && git push -u origin main

EOF
