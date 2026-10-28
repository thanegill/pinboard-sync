//! The shells `completions` prints a script for, and the scripts.
//!
//! bpaf completes dynamically: each script hands the words typed so far back to the
//! binary, behind a `--bpaf-complete-rev=N` that names the output format, and the binary
//! answers with the candidates. The scripts follow bpaf 0.9's own (`complete_run.rs`),
//! which it only prints and exits on, but pass the words as arguments where bpaf's bash,
//! zsh and fish scripts `eval` them: an `eval` runs a `$(...)` typed on the line, and
//! bash's writes to a redirection target, each time TAB is pressed. Without it bash and zsh
//! keep any quotes typed around a word, and the command run is expanded only for a
//! leading `~/`, elvish's not at all. zsh itself still expands the word being completed.

use std::fmt;
use std::str::FromStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    Elvish,
}

impl Shell {
    pub const ALL: [Shell; 4] = [Shell::Bash, Shell::Zsh, Shell::Fish, Shell::Elvish];

    fn name(self) -> &'static str {
        match self {
            Shell::Bash => "bash",
            Shell::Zsh => "zsh",
            Shell::Fish => "fish",
            Shell::Elvish => "elvish",
        }
    }

    /// The completion script for the binary `name`.
    pub fn script(self, name: &str) -> String {
        match self {
            Shell::Bash => format!(
                r#"_bpaf_dynamic_completion()
{{
    local _bpaf_command=$1 _bpaf_words=("${{COMP_WORDS[@]:1:COMP_CWORD}}")
    [[ $_bpaf_command == "~/"* ]] && _bpaf_command=$HOME/${{_bpaf_command#"~/"}}
    source <("$_bpaf_command" --bpaf-complete-rev=8 "${{_bpaf_words[@]}}")
}}
complete -o nosort -F _bpaf_dynamic_completion {name}
"#
            ),
            Shell::Zsh => format!(
                r#"#compdef {name}
source <("${{words[1]/#\~\//$HOME/}}" --bpaf-complete-rev=7 "${{(@)words[2,CURRENT]}}")
"#
            ),
            Shell::Fish => format!(
                r#"function _bpaf_dynamic_completion
    set -l current (commandline --tokenize --current-process)
    set -l command $current[1]
    string match -q -- '~/*' $command; and set command $HOME/(string sub -s 3 -- $command)
    set -l args --bpaf-complete-rev=9 $current[2..]
    if test (commandline --current-process) != (string trim (commandline --current-process))
        set args $args ""
    end
    $command $args
end

complete --no-files --command {name} --arguments '(_bpaf_dynamic_completion)'
"#
            ),
            Shell::Elvish => format!(
                "\
set edit:completion:arg-completer[{name}] = {{ |@args| var args = $args[1..];
     var @lines = ( {name} --bpaf-complete-rev=1 $@args );
     use str;
     for line $lines {{
         var @arg = (str:split \"\\t\" $line)
         try {{
             edit:complex-candidate $arg[0] &display=( printf \"%-19s %s\" $arg[0] $arg[1] )
         }} catch {{
             edit:complex-candidate $line
         }}
     }}
}}
"
            ),
        }
    }

    /// The shell names starting with `typed`, for completing the `completions` argument.
    pub fn complete(typed: &str) -> Vec<(&'static str, Option<&'static str>)> {
        Shell::ALL
            .iter()
            .map(|shell| shell.name())
            .filter(|name| name.starts_with(typed))
            .map(|name| (name, None))
            .collect()
    }
}

impl fmt::Display for Shell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Shell {
    type Err = &'static str;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Shell::ALL
            .into_iter()
            .find(|shell| shell.name() == name)
            .ok_or("the shell must be one of bash, zsh, fish or elvish")
    }
}
