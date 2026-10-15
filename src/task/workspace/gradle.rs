use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use eyre::{Context, Result};

use super::{
    ProjectId, WorkspaceDiscoveryContext, WorkspaceProject, WorkspaceProvenance, WorkspaceProvider,
};

const SETTINGS_KOTLIN: &str = "settings.gradle.kts";
const SETTINGS_GROOVY: &str = "settings.gradle";
const BUILD_KOTLIN: &str = "build.gradle.kts";
const BUILD_GROOVY: &str = "build.gradle";

/// Discovers Gradle projects listed by a settings script.
///
/// Gradle declares its members with `include` directives in `settings.gradle`
/// or `settings.gradle.kts`. This provider reads that script statically and
/// never runs Gradle, the wrapper, or the Tooling API.
///
/// Gradle is the first ecosystem whose structure lives in an executable build
/// script rather than in data, so membership that is not a literal string — an
/// interpolated, looped, or otherwise computed `include` — is declined rather
/// than guessed at. A partial project list would make `--affected` silently
/// wrong, which is worse than reporting no projects at all. Edges are treated
/// as more forgiving: a project whose dependencies cannot be read confidently
/// is still reported, with no edges, and `[monorepo.projects] depends_add` can
/// supply them.
#[derive(Debug, Default)]
pub struct GradleWorkspaceProvider;

impl WorkspaceProvider for GradleWorkspaceProvider {
    fn id(&self) -> &str {
        "gradle"
    }

    fn discover(&self, workspace_root: &Path) -> Result<Vec<WorkspaceProject>> {
        let context = WorkspaceDiscoveryContext::new();
        self.discover_with_context(workspace_root, &context)
    }

    fn discover_with_context(
        &self,
        workspace_root: &Path,
        context: &WorkspaceDiscoveryContext,
    ) -> Result<Vec<WorkspaceProject>> {
        let Some((settings_name, settings_path)) = settings_script(context, workspace_root) else {
            return Ok(Vec::new());
        };
        let contents = context.read_to_string(&settings_path).wrap_err_with(|| {
            format!(
                "failed to read Gradle settings script {}",
                settings_path.display()
            )
        })?;
        let Some(settings) = parse_settings(&contents) else {
            return Ok(Vec::new());
        };
        if settings.members.is_empty() {
            return Ok(Vec::new());
        }

        // Gradle project path -> workspace-relative directory.
        let mut roots = BTreeMap::new();
        for member in &settings.members {
            let directory = settings
                .remapped
                .get(member)
                .cloned()
                .unwrap_or_else(|| project_directory(member));
            roots.insert(member.clone(), directory);
        }
        // The root project only participates when it has a build script of its
        // own, mirroring how the Cargo provider treats a workspace root package.
        if build_script(context, workspace_root, Path::new(".")).is_some() {
            roots.insert(":".to_string(), PathBuf::from("."));
        }

        let mut ids = BTreeMap::new();
        for member in roots.keys() {
            ids.insert(member.clone(), ProjectId::new(self.id(), member)?);
        }

        let mut projects = Vec::new();
        for (member, directory) in &roots {
            let Some(id) = ids.get(member) else {
                continue;
            };
            let mut project = WorkspaceProject::new(id.clone(), directory.clone());
            project
                .metadata
                .insert("workspace_source".to_string(), settings_name.clone());

            let script = build_script(context, workspace_root, directory);
            let source = script
                .as_ref()
                .and_then(|path| path.strip_prefix(workspace_root).ok())
                .map_or_else(|| PathBuf::from(&settings_name), Path::to_path_buf);
            project.provenance = WorkspaceProvenance {
                provider: Some(self.id().to_string()),
                source: Some(source.clone()),
            };

            if let Some(script) = script {
                let script_contents = context.read_to_string(&script).wrap_err_with(|| {
                    format!("failed to read Gradle build script {}", script.display())
                })?;
                for referenced in project_references(&script_contents) {
                    let Some(dependency) = ids.get(&referenced) else {
                        continue;
                    };
                    if dependency == id {
                        continue;
                    }
                    project.dependencies.insert(dependency.clone());
                    project.dependency_provenance.insert(
                        dependency.clone(),
                        WorkspaceProvenance {
                            provider: Some(self.id().to_string()),
                            source: Some(source.clone()),
                        },
                    );
                }
            }

            projects.push(project);
        }

        Ok(projects)
    }
}

/// A statically parsed settings script.
#[derive(Debug, Default, Eq, PartialEq)]
struct GradleSettings {
    /// Member project paths in declaration order, such as `:app`.
    members: Vec<String>,
    /// Member paths whose directory is set explicitly, keyed by project path.
    remapped: BTreeMap<String, PathBuf>,
}

/// Parses `include` directives and `projectDir` remaps out of a settings script.
///
/// Returns `None` when membership is not statically enumerable, so the caller
/// can decline rather than report a partial project list.
fn parse_settings(contents: &str) -> Option<GradleSettings> {
    let mut settings = GradleSettings::default();

    for (index, identifier) in identifiers(contents) {
        if is_property_access(contents, index) {
            continue;
        }
        let after = index + identifier.len();
        match identifier.as_str() {
            "include" => {
                let (_, arguments) = call_span(contents, after)?;
                for argument in &arguments {
                    let path = string_literal(argument)?;
                    if !path.starts_with(':') {
                        return None;
                    }
                    if !settings.members.contains(&path) {
                        settings.members.push(path);
                    }
                }
            }
            "project" => {
                if let Some((path, directory)) = project_remap(contents, after) {
                    settings.remapped.insert(path, directory);
                }
            }
            _ => {}
        }
    }

    Some(settings)
}

/// Reads the member paths referenced by a build script's `project(...)` calls.
fn project_references(contents: &str) -> Vec<String> {
    let mut references = Vec::new();

    for (index, identifier) in identifiers(contents) {
        if identifier != "project" || is_property_access(contents, index) {
            continue;
        }
        let after = index + identifier.len();
        let Some((_, arguments)) = call_span(contents, after) else {
            continue;
        };
        for argument in &arguments {
            let Some(value) = labelled_literal(argument) else {
                continue;
            };
            if value.starts_with(':') && !references.contains(&value) {
                references.push(value);
            }
        }
    }

    references
}

/// Parses `project(":app").projectDir = file("modules/app")`.
fn project_remap(contents: &str, after: usize) -> Option<(String, PathBuf)> {
    let (end, arguments) = call_span(contents, after)?;
    let path = arguments
        .iter()
        .filter_map(|argument| labelled_literal(argument))
        .find(|value| value.starts_with(':'))?;

    let rest = contents.get(end..)?.trim_start();
    let rest = rest.strip_prefix(".projectDir")?.trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let inner = rest.strip_prefix("file")?.trim_start();
    let inner = inner.strip_prefix('(')?;
    let close = closing_parenthesis(inner)?;
    let directory = string_literal(&inner[..close])?;

    Some((path, PathBuf::from(directory)))
}

/// Returns the raw arguments of a call starting at `after`, and the offset just
/// past the call.
fn call_span(contents: &str, after: usize) -> Option<(usize, Vec<String>)> {
    let rest = contents.get(after..)?;
    let trimmed = rest.trim_start();
    let skipped = rest.len() - trimmed.len();

    if let Some(inner) = trimmed.strip_prefix('(') {
        let end = closing_parenthesis(inner)?;
        let arguments = split_arguments(&inner[..end])?;
        return Some((after + skipped + end + 2, arguments));
    }

    // Groovy command syntax: `include ':app', ':lib'` runs to the end of the line.
    let line = trimmed.split('\n').next().unwrap_or_default();
    let line = line.trim().trim_end_matches(';');
    let arguments = split_arguments(line)?;
    Some((after + skipped + line.len(), arguments))
}

/// Finds the index of the parenthesis closing the one already consumed.
fn closing_parenthesis(text: &str) -> Option<usize> {
    let characters = text.chars().collect::<Vec<_>>();
    let mut depth = 1usize;
    let mut index = 0usize;

    while index < characters.len() {
        let character = characters[index];
        match character {
            '\'' | '"' => {
                index += 1;
                while index < characters.len() {
                    if characters[index] == '\\' {
                        index += 2;
                        continue;
                    }
                    if characters[index] == character {
                        break;
                    }
                    index += 1;
                }
                index += 1;
            }
            '(' => {
                depth += 1;
                index += 1;
            }
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
                index += 1;
            }
            _ => index += 1,
        }
    }

    None
}

/// Splits a call's arguments on top-level commas.
fn split_arguments(text: &str) -> Option<Vec<String>> {
    let characters = text.chars().collect::<Vec<_>>();
    let mut arguments = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;
    let mut index = 0usize;

    while index < characters.len() {
        let character = characters[index];
        match character {
            '\'' | '"' => {
                current.push(character);
                index += 1;
                while index < characters.len() {
                    let inner = characters[index];
                    current.push(inner);
                    index += 1;
                    if inner == '\\' && index < characters.len() {
                        current.push(characters[index]);
                        index += 1;
                        continue;
                    }
                    if inner == character {
                        break;
                    }
                }
            }
            '(' | '[' | '{' => {
                depth += 1;
                current.push(character);
                index += 1;
            }
            ')' | ']' | '}' => {
                depth -= 1;
                current.push(character);
                index += 1;
            }
            ',' if depth == 0 => {
                arguments.push(current.trim().to_string());
                current.clear();
                index += 1;
            }
            '/' if characters.get(index + 1) == Some(&'/') => {
                while index < characters.len() && characters[index] != '\n' {
                    index += 1;
                }
            }
            '/' if characters.get(index + 1) == Some(&'*') => {
                index += 2;
                while index < characters.len()
                    && !(characters[index] == '*' && characters.get(index + 1) == Some(&'/'))
                {
                    index += 1;
                }
                index = (index + 2).min(characters.len());
            }
            _ => {
                current.push(character);
                index += 1;
            }
        }
    }

    if !current.trim().is_empty() {
        arguments.push(current.trim().to_string());
    }

    Some(arguments)
}

/// Extracts a plain string literal, declining interpolation and escapes.
fn string_literal(argument: &str) -> Option<String> {
    let argument = argument.trim();
    let quote = argument.chars().next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    if argument.len() < 2 || !argument.ends_with(quote) {
        return None;
    }
    let value = &argument[1..argument.len() - 1];
    if value.contains('$') || value.contains('\\') {
        return None;
    }
    Some(value.to_string())
}

/// Extracts a string literal from a positional or labelled argument.
fn labelled_literal(argument: &str) -> Option<String> {
    let argument = argument.trim();
    if let Some(value) = string_literal(argument) {
        return Some(value);
    }
    let (_, value) = argument
        .split_once(':')
        .or_else(|| argument.split_once('='))?;
    string_literal(value)
}

/// Returns the identifier-like words outside strings and comments.
fn identifiers(contents: &str) -> Vec<(usize, String)> {
    let bytes = contents.as_bytes();
    let mut found = Vec::new();
    let mut index = 0usize;

    while index < bytes.len() {
        match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                while index < bytes.len()
                    && !(bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/'))
                {
                    index += 1;
                }
                index = (index + 2).min(bytes.len());
            }
            quote @ (b'\'' | b'"') => {
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == b'\\' {
                        index += 2;
                        continue;
                    }
                    if bytes[index] == quote {
                        index += 1;
                        break;
                    }
                    index += 1;
                }
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' => {
                let start = index;
                while index < bytes.len()
                    && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
                {
                    index += 1;
                }
                found.push((start, contents[start..index].to_string()));
            }
            _ => index += 1,
        }
    }

    found
}

fn is_property_access(contents: &str, index: usize) -> bool {
    contents
        .get(..index)
        .is_some_and(|prefix| prefix.trim_end().ends_with('.'))
}

fn settings_script(
    context: &WorkspaceDiscoveryContext,
    workspace_root: &Path,
) -> Option<(String, PathBuf)> {
    // Gradle's documented behaviour when both scripts are present is not
    // specified, so the Kotlin DSL is preferred here and this is an open
    // question for the maintainer.
    for name in [SETTINGS_KOTLIN, SETTINGS_GROOVY] {
        let path = workspace_root.join(name);
        if context.is_file(&path) {
            return Some((name.to_string(), path));
        }
    }
    None
}

fn build_script(
    context: &WorkspaceDiscoveryContext,
    workspace_root: &Path,
    directory: &Path,
) -> Option<PathBuf> {
    let root = if directory.as_os_str().is_empty() || directory == Path::new(".") {
        workspace_root.to_path_buf()
    } else {
        workspace_root.join(directory)
    };

    for name in [BUILD_KOTLIN, BUILD_GROOVY] {
        let path = root.join(name);
        if context.is_file(&path) {
            return Some(path);
        }
    }
    None
}

/// Converts a Gradle project path such as `:a:b` to a workspace-relative directory.
fn project_directory(path: &str) -> PathBuf {
    let trimmed = path.strip_prefix(':').unwrap_or(path);
    if trimmed.is_empty() {
        return PathBuf::from(".");
    }
    trimmed.split(':').collect()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn settings(root: &Path, contents: &str) {
        write(&root.join(SETTINGS_KOTLIN), contents);
    }

    fn summary(projects: &[WorkspaceProject]) -> Vec<(&str, &Path)> {
        projects
            .iter()
            .map(|project| (project.id.as_str(), project.root.as_path()))
            .collect()
    }

    #[test]
    fn discovers_members_and_dependency_edges() {
        let temp = tempdir().unwrap();
        settings(
            temp.path(),
            "rootProject.name = \"acme\"\ninclude(\":app\", \":lib\")\n",
        );
        write(
            &temp.path().join("app/build.gradle.kts"),
            "dependencies {\n    implementation(project(\":lib\"))\n}\n",
        );

        let projects = GradleWorkspaceProvider.discover(temp.path()).unwrap();

        assert_eq!(
            summary(&projects),
            vec![("gradle::app", Path::new("app")), ("gradle::lib", Path::new("lib"))]
        );
        let app = projects
            .iter()
            .find(|project| project.id.as_str() == "gradle::app")
            .unwrap();
        assert_eq!(
            app.dependencies
                .iter()
                .map(ProjectId::as_str)
                .collect::<Vec<_>>(),
            vec!["gradle::lib"]
        );
        assert_eq!(app.metadata.get("workspace_source").unwrap(), SETTINGS_KOTLIN);
        assert_eq!(
            app.provenance.source,
            Some(PathBuf::from("app/build.gradle.kts"))
        );
    }

    #[test]
    fn supports_groovy_command_syntax_and_nested_paths() {
        let temp = tempdir().unwrap();
        write(
            &temp.path().join(SETTINGS_GROOVY),
            "include ':services:person-service', ':lib' // members\n",
        );

        let projects = GradleWorkspaceProvider.discover(temp.path()).unwrap();

        assert_eq!(
            summary(&projects),
            vec![
                ("gradle::lib", Path::new("lib")),
                (
                    "gradle::services:person-service",
                    Path::new("services/person-service")
                ),
            ]
        );
        assert_eq!(
            projects[0].metadata.get("workspace_source").unwrap(),
            SETTINGS_GROOVY
        );
    }

    #[test]
    fn declines_computed_membership() {
        let temp = tempdir().unwrap();
        settings(
            temp.path(),
            "file(\"modules\").listFiles()?.forEach { include(it.name) }\n",
        );

        assert!(GradleWorkspaceProvider.discover(temp.path()).unwrap().is_empty());
    }

    #[test]
    fn declines_interpolated_membership() {
        let temp = tempdir().unwrap();
        settings(temp.path(), "include(\"$rootDir/app\")\n");

        assert!(GradleWorkspaceProvider.discover(temp.path()).unwrap().is_empty());
    }

    #[test]
    fn declines_a_single_non_literal_member_among_literals() {
        let temp = tempdir().unwrap();
        settings(temp.path(), "include(\":app\")\ninclude(dynamic)\n");

        assert!(GradleWorkspaceProvider.discover(temp.path()).unwrap().is_empty());
    }

    #[test]
    fn ignores_include_build() {
        let temp = tempdir().unwrap();
        settings(temp.path(), "includeBuild(\"../other\")\n");

        assert!(GradleWorkspaceProvider.discover(temp.path()).unwrap().is_empty());
    }

    #[test]
    fn ignores_roots_without_a_settings_script() {
        let temp = tempdir().unwrap();
        write(&temp.path().join("build.gradle.kts"), "plugins {}\n");

        assert!(GradleWorkspaceProvider.discover(temp.path()).unwrap().is_empty());
    }

    #[test]
    fn includes_the_root_project_only_when_it_has_a_build_script() {
        let temp = tempdir().unwrap();
        settings(temp.path(), "include(\":app\")\n");

        let projects = GradleWorkspaceProvider.discover(temp.path()).unwrap();
        assert_eq!(summary(&projects), vec![("gradle::app", Path::new("app"))]);

        write(&temp.path().join("build.gradle.kts"), "plugins {}\n");
        let projects = GradleWorkspaceProvider.discover(temp.path()).unwrap();
        assert_eq!(
            summary(&projects),
            vec![("gradle::", Path::new(".")), ("gradle::app", Path::new("app"))]
        );
    }

    #[test]
    fn resolves_explicit_project_directories() {
        let temp = tempdir().unwrap();
        settings(
            temp.path(),
            "include(\":app\")\nproject(\":app\").projectDir = file(\"modules/app\")\n",
        );

        let projects = GradleWorkspaceProvider.discover(temp.path()).unwrap();

        assert_eq!(
            summary(&projects),
            vec![("gradle::app", Path::new("modules/app"))]
        );
    }

    #[test]
    fn skips_self_edges_and_unresolved_references() {
        let temp = tempdir().unwrap();
        settings(temp.path(), "include(\":app\")\n");
        write(
            &temp.path().join("app/build.gradle.kts"),
            "dependencies {\n    implementation(project(\":app\"))\n    \
             implementation(project(\":missing\"))\n    \
             implementation(\"com.example:external:1.0\")\n}\n",
        );

        let projects = GradleWorkspaceProvider.discover(temp.path()).unwrap();

        assert!(projects[0].dependencies.is_empty());
    }

    #[test]
    fn reads_labelled_project_references() {
        let temp = tempdir().unwrap();
        settings(temp.path(), "include(\":app\", \":lib\")\n");
        write(
            &temp.path().join("app/build.gradle"),
            "dependencies {\n    implementation project(path: ':lib', configuration: 'default')\n}\n",
        );

        let projects = GradleWorkspaceProvider.discover(temp.path()).unwrap();
        let app = projects
            .iter()
            .find(|project| project.id.as_str() == "gradle::app")
            .unwrap();

        assert_eq!(
            app.dependencies
                .iter()
                .map(ProjectId::as_str)
                .collect::<Vec<_>>(),
            vec!["gradle::lib"]
        );
    }

    #[test]
    fn ignores_identifiers_inside_comments_and_strings() {
        let temp = tempdir().unwrap();
        settings(
            temp.path(),
            "// include(\":commented-out\")\nval label = \"include(':string')\"\ninclude(\":app\")\n",
        );

        let projects = GradleWorkspaceProvider.discover(temp.path()).unwrap();

        assert_eq!(summary(&projects), vec![("gradle::app", Path::new("app"))]);
    }

    #[test]
    fn ignores_settings_without_any_members() {
        let temp = tempdir().unwrap();
        settings(temp.path(), "rootProject.name = \"single\"\n");

        assert!(GradleWorkspaceProvider.discover(temp.path()).unwrap().is_empty());
    }

    #[test]
    fn converts_project_paths_to_directories() {
        assert_eq!(project_directory(":"), PathBuf::from("."));
        assert_eq!(project_directory(":app"), PathBuf::from("app"));
        assert_eq!(
            project_directory(":a:b:c"),
            PathBuf::from("a").join("b").join("c")
        );
    }
}
