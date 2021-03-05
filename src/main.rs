#[macro_use]
extern crate clap;
extern crate git2;

use git2::{Commit, Index, Oid, Repository, Revwalk, Tree};
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[macro_use]
mod macros;

const E_SUCCESS: i32 = 0;
const E_NO_GIT_REPO: i32 = 1;
const E_FOUND_DANGLING_REFERENCES: i32 = 2;
const E_INVALID_COMMIT_ID: i32 = 3;
const E_INVALID_MAPPINGS: i32 = 4;
const E_DIRTY_WORKDIR: i32 = 5;
const E_SUBMODULE_FETCH_FAILED: i32 = 6;
const E_SUBMODULE_NOT_FOUND: i32 = 7;

fn main() {
    let exit_code = real_main();
    std::process::exit(exit_code);
}

fn real_main() -> i32 {
    let mut mappings: HashMap<Oid, Oid> = HashMap::new();
    let (submodule_dir, default_mapping) = match parse_cli_arguments(&mut mappings) {
        Ok((dir, oid)) => (dir, oid),
        Err(exit_code) => return exit_code,
    };

    let repo = match Repository::open(".") {
        Ok(repo) => repo,
        Err(e) => {
            eprintln!(
                "Couldn't find Git repo in the current directory: {}",
                e.message()
            );
            return E_NO_GIT_REPO;
        }
    };

    if !is_workdir_clean(&repo) {
        eprintln!("The working directory is dirty, aborting!");
        return E_DIRTY_WORKDIR;
    }

    if !does_submodule_exist(&repo, &submodule_dir) {
        eprintln!("Couldn't find a submodule named `{}'", submodule_dir);
        return E_SUBMODULE_NOT_FOUND;
    }

    match fetch_submodule_history(&repo, &submodule_dir) {
        Ok(_) => {}
        Err(_) => return E_SUBMODULE_FETCH_FAILED,
    }

    if !are_mappings_valid(&repo, &submodule_dir, &mappings, &default_mapping) {
        return E_INVALID_MAPPINGS;
    }

    println!("Merging {}...", submodule_dir);

    let mut old_id_to_new = HashMap::new();

    rewrite_submodule_history(&repo, &mut old_id_to_new, &submodule_dir);

    match find_dangling_references_to_submodule(
        &repo,
        &submodule_dir,
        &old_id_to_new,
        &mappings,
        &default_mapping,
    ) {
        Some(_) => return E_FOUND_DANGLING_REFERENCES,
        None => {}
    }

    rewrite_repo_history(
        &repo,
        &mut old_id_to_new,
        &mappings,
        &default_mapping,
        &submodule_dir,
    );

    // Working directories with and without submodules are pretty much
    // the same, save for two files:
    // - submodules have .git in their root directory;
    // - there's .gitmodules in the root of the repo.
    remove_dotgit_from_submodule(&submodule_dir);
    remove_gitmodules();
    // Git used to think of submodule's directory as a file, because it was
    // "opaque". We have to update the index in order for Git to realise
    // that the submodule directory is *just* a directory now.
    update_index(&repo, &old_id_to_new);

    E_SUCCESS
}

fn parse_cli_arguments(mappings: &mut HashMap<Oid, Oid>) -> Result<(String, Option<Oid>), i32> {
    let options = clap::App::new("git-submerge")
        .version("0.5")
        .author(crate_authors!())
        .about("Merge Git submodule into the main repo as if they've never been separate at all")
        .arg(
            clap::Arg::with_name("SUBMODULE_DIR")
                .help("The submodule to merge")
                .required(true)
                .index(1),
        )
        .arg(
            clap::Arg::with_name("mapping")
                .value_names(&["commit id 1", "commit id 2"])
                .help(
                    "Whenever main repo references submodule's <commit id 1>, the <commit id 2> \
                   will be used instead",
                )
                .short("m")
                .long("mapping")
                .number_of_values(2)
                .multiple(true),
        )
        .arg(
            clap::Arg::with_name("default-mapping")
                .value_name("commit id")
                .help(
                    "Whenever main repo references a commit that is neither in submodule's \
                   history nor in mappings (see --mapping), the <commit id> will be used instead",
                )
                .short("d")
                .long("default-mapping")
                .number_of_values(1)
                .multiple(false),
        )
        .get_matches();

    match options.values_of("mapping") {
        None => {}
        Some(values) => {
            let mut i: i32 = 1;
            let (first, second): (Vec<&str>, Vec<&str>) = values.partition(|_| {
                i += 1;
                i % 2 == 0
            });
            for (f, s) in first.iter().zip(second.iter()) {
                let oid1 = match Oid::from_str(f) {
                    Ok(oid) => oid,
                    Err(_) => {
                        eprintln!("{} is not a valid 40-character hex string", f);
                        return Err(E_INVALID_COMMIT_ID);
                    }
                };

                let oid2 = match Oid::from_str(s) {
                    Ok(oid) => oid,
                    Err(_) => {
                        eprintln!("{} is not a valid 40-character hex string", s);
                        return Err(E_INVALID_COMMIT_ID);
                    }
                };

                mappings.insert(oid1, oid2);
            }
        }
    }

    let default_mapping_str = options.value_of("default-mapping");
    let default_mapping = if let Some(s) = default_mapping_str {
        match Oid::from_str(s) {
            Ok(oid) => Some(oid),
            Err(_) => {
                eprintln!("{} is not a valid 40-character hex string", s);
                return Err(E_INVALID_COMMIT_ID);
            }
        }
    } else {
        None
    };

    // We can safely use unwrap() here because the argument is marked as "required" and Clap checks
    // its presence for us.
    Ok((
        String::from(options.value_of("SUBMODULE_DIR").unwrap()),
        default_mapping,
    ))
}

fn is_workdir_clean(repo: &Repository) -> bool {
    let mut statusopts = git2::StatusOptions::new();
    statusopts.include_untracked(false);
    statusopts.include_ignored(false);
    statusopts.include_unmodified(false);
    statusopts.exclude_submodules(false);
    statusopts.recurse_untracked_dirs(false);
    statusopts.recurse_ignored_dirs(false);
    let statuses = repo
        .statuses(Some(&mut statusopts))
        .expect("Couldn't get statuses from the repo");
    statuses.iter().count() == 0
}

fn does_submodule_exist(repo: &Repository, submodule_dir: &str) -> bool {
    repo.find_submodule(submodule_dir).is_ok()
}

// Checks if all the values in the `mappings` exist in submodule's history
fn are_mappings_valid(
    repo: &Repository,
    submodule_dir: &str,
    mappings: &HashMap<Oid, Oid>,
    default_mapping: &Option<Oid>,
) -> bool {
    let mut commits: HashSet<Oid> = mappings.values().cloned().collect();
    if let &Some(oid) = default_mapping {
        commits.insert(oid);
    };

    let revwalk = get_submodule_revwalk(&repo, &submodule_dir);
    for maybe_oid in revwalk {
        match maybe_oid {
            Ok(oid) => {
                commits.remove(&oid);
            }
            Err(e) => eprintln!("Error walking the submodule's history: {:?}", e),
        }
    }

    for commit in commits.iter() {
        eprintln!("Commit {} not found in submodule's history.", commit);
    }

    commits.len() == 0
}

fn get_submodule_revwalk<'repo>(repo: &'repo Repository, submodule_dir: &str) -> Revwalk<'repo> {
    let submodule = repo
        .find_submodule(submodule_dir)
        .expect("Couldn't find the submodule with expected path");
    let submodule_head = submodule
        .head_id()
        .expect("Couldn't obtain submodule's HEAD");

    let mut revwalk = repo
        .revwalk()
        .expect("Couldn't obtain RevWalk object for the repo");
    // "Topological" and reverse means "parents are always visited before their children".
    // We need that in order to be sure that our old-to-new-ids map always contains everything we
    // need it to contain.
    revwalk.set_sorting(git2::SORT_REVERSE | git2::SORT_TOPOLOGICAL);
    // TODO (#6): push all branches and tags, not just HEAD
    revwalk
        .push(submodule_head)
        .expect("Couldn't add submodule's HEAD to RevWalk");

    revwalk
}

fn fetch_submodule_history(repo: &Repository, submodule_dir: &str) -> Result<(), ()> {
    let submodule_url = String::from("./") + submodule_dir;
    let mut remote = repo
        .remote_anonymous(&submodule_url)
        .expect("Couldn't create an anonymous remote");
    match remote.fetch(&[], None, None) {
        Ok(_) => Ok(()),
        Err(_) => {
            eprintln!(
                "Couldn't fetch submodule's history!  Have you forgot to run \
                       `git submodule update --recursive`?"
            );
            Err(())
        }
    }
}

fn rewrite_submodule_history(
    repo: &Repository,
    old_id_to_new: &mut HashMap<Oid, Oid>,
    submodule_dir: &str,
) {
    let revwalk = get_submodule_revwalk(&repo, &submodule_dir);
    for maybe_oid in revwalk {
        match maybe_oid {
            Ok(oid) => {
                let commit = repo
                    .find_commit(oid)
                    .expect(&format!("Couldn't get a commit with ID {}", oid));
                let tree = commit.tree().expect(&format!(
                    "Couldn't obtain the tree of a commit with ID {}",
                    oid
                ));
                let mut old_index =
                    Index::new().expect("Couldn't create an in-memory index for commit");
                let mut new_index = Index::new().expect("Couldn't create an in-memory index");
                old_index
                    .read_tree(&tree)
                    .expect(&format!("Couldn't read the commit {} into index", oid));

                // Obtain the new tree, where everything from the old one is moved under
                // a directory named after the submodule
                for entry in old_index.iter() {
                    let mut new_entry = entry;

                    let mut new_path = String::from(submodule_dir);
                    new_path += "/";
                    new_path += &String::from_utf8(new_entry.path)
                        .expect("Failed to convert a path to str");

                    new_entry.path = new_path.into_bytes();
                    new_index
                        .add(&new_entry)
                        .expect("Couldn't add an entry to the index");
                }
                let tree_id = new_index
                    .write_tree_to(&repo)
                    .expect("Couldn't write the index into a tree");
                old_id_to_new.insert(tree.id(), tree_id);
                let tree = repo
                    .find_tree(tree_id)
                    .expect("Couldn't retrieve the tree we just created");

                let parents = {
                    let mut p: Vec<Commit> = Vec::new();
                    for parent_id in commit.parent_ids() {
                        let new_parent_id = old_id_to_new[&parent_id];
                        let parent = repo
                            .find_commit(new_parent_id)
                            .expect("Couldn't find parent commit by its id");
                        p.push(parent);
                    }
                    p
                };

                let mut parents_refs: Vec<&Commit> = Vec::new();
                for i in 0..parents.len() {
                    parents_refs.push(&parents[i]);
                }
                let new_commit_id = repo
                    .commit(
                        None,
                        &commit.author(),
                        &commit.committer(),
                        &commit
                            .message()
                            .expect("Couldn't retrieve commit's message"),
                        &tree,
                        &parents_refs[..],
                    )
                    .expect("Failed to commit");

                old_id_to_new.insert(oid, new_commit_id);
            }
            Err(e) => eprintln!("Error walking the submodule's history: {:?}", e),
        }
    }
}

fn find_dangling_references_to_submodule<'repo>(
    repo: &'repo Repository,
    submodule_dir: &str,
    old_id_to_new: &HashMap<Oid, Oid>,
    mappings: &HashMap<Oid, Oid>,
    default_mapping: &Option<Oid>,
) -> Option<bool> {
    let submodule_path = Path::new(submodule_dir);

    let known_submodule_commits: HashSet<&Oid> = old_id_to_new.keys().collect();
    let mut dangling_references = HashSet::new();

    let revwalk = get_repo_revwalk(&repo);

    for maybe_oid in revwalk {
        match maybe_oid {
            Ok(oid) => {
                let commit = repo
                    .find_commit(oid)
                    .expect(&format!("Couldn't get a commit with ID {}", oid));
                let tree = commit.tree().expect(&format!(
                    "Couldn't obtain the tree of a commit with ID {}",
                    oid
                ));

                let submodule_subdir = match tree.get_path(submodule_path) {
                    Ok(tree) => {
                        // We're only interested in gitlinks
                        if tree.filemode() != 0o160000 {
                            continue;
                        }
                        tree
                    }
                    Err(e) => {
                        if e.code() == git2::ErrorCode::NotFound
                            && e.class() == git2::ErrorClass::Tree
                        {
                            // It's okay. The tree lacks the subtree corresponding to the
                            // submodule. In other words, the commit doesn't include the submodule.
                            // That's totally fine. Let's  move on.
                            continue;
                        } else {
                            // Unexpected error; let's report it and abort the program
                            panic!("Error getting submodule's subdir from the tree: {:?}", e);
                        };
                    }
                };

                // **INVARIANT**: if we got this far, current commit contains a submodule and
                // should be rewritten

                let submodule_commit_id = submodule_subdir.id();
                if !known_submodule_commits.contains(&submodule_commit_id)
                    && !mappings.contains_key(&submodule_commit_id)
                    && default_mapping.is_none()
                {
                    dangling_references.insert(submodule_commit_id);
                }
            }
            Err(e) => eprintln!("Error walking the submodule's history: {:?}", e),
        }
    }

    if dangling_references.is_empty() {
        None
    } else {
        eprintln!(
            "The repository references the following submodule commits, but they couldn't \
                   be found in the submodule's history:\n"
        );
        for id in dangling_references {
            eprintln!("{}", id);
        }

        eprintln!(
            "\nYou can use --mapping and --default-mapping options to make git-submerge \
                   replace these commits with some other, still existing, commits."
        );

        Some(true)
    }
}

fn get_repo_revwalk<'repo>(repo: &'repo Repository) -> Revwalk<'repo> {
    let mut revwalk = repo
        .revwalk()
        .expect("Couldn't obtain RevWalk object for the repo");
    revwalk.set_sorting(git2::SORT_REVERSE | git2::SORT_TOPOLOGICAL);
    let head = repo.head().expect("Couldn't obtain repo's HEAD");
    let head_id = head
        .target()
        .expect("Couldn't resolve repo's HEAD to a commit ID");
    revwalk
        .push(head_id)
        .expect("Couldn't add repo's HEAD to RevWalk");

    for (name, id) in get_branch_to_id_map(&repo) {
        revwalk
            .push(id)
            .expect(&format!("Couldn't push branch `{}' to RevWalk", name));
    }

    revwalk
}

fn get_branch_to_id_map(repo: &Repository) -> HashMap<String, Oid> {
    let mut result = HashMap::new();

    let branches = repo
        .branches(Some(git2::BranchType::Local))
        .expect("Couldn't obtain an iterator over local branches");
    for maybe_branch in branches {
        match maybe_branch {
            Ok((branch, _)) => {
                let name = branch
                    .name()
                    .expect("Couldn't get branch' name")
                    .expect("Branch name is not valid UTF-8");
                let id = branch
                    .get()
                    .peel(git2::ObjectType::Commit)
                    .expect("Couldn't convert branch into a Commit")
                    .id();
                result.insert(String::from(name), id);
            }
            Err(e) => eprintln!("Error walking the branches: {:?}", e),
        }
    }

    result
}

fn rewrite_repo_history(
    repo: &Repository,
    old_id_to_new: &mut HashMap<Oid, Oid>,
    mappings: &HashMap<Oid, Oid>,
    default_mapping: &Option<Oid>,
    submodule_dir: &str,
) {
    let revwalk = get_repo_revwalk(&repo);
    let submodule_path = Path::new(submodule_dir);

    for maybe_oid in revwalk {
        match maybe_oid {
            Ok(oid) => {
                let commit = repo
                    .find_commit(oid)
                    .expect(&format!("Couldn't get a commit with ID {}", oid));
                let tree = commit.tree().expect(&format!(
                    "Couldn't obtain the tree of a commit with ID {}",
                    oid
                ));

                let submodule_subdir = match tree.get_path(submodule_path) {
                    Ok(tree) => {
                        // We're only interested in gitlinks
                        if tree.filemode() != 0o160000 {
                            continue;
                        };
                        tree
                    }
                    Err(e) => {
                        if e.code() == git2::ErrorCode::NotFound
                            && e.class() == git2::ErrorClass::Tree
                        {
                            // It's okay. The tree lacks the subtree corresponding to the
                            // submodule. In other words, the commit doesn't include the submodule.
                            // That's totally fine. Let's map it into itself and move on.
                            old_id_to_new.insert(oid, oid);
                            continue;
                        } else {
                            // Unexpected error; let's report it and abort the program
                            panic!("Error getting submodule's subdir from the tree: {:?}", e);
                        };
                    }
                };

                // **INVARIANT**: if we got this far, current commit contains a submodule and
                // should be rewritten

                let submodule_commit_id = submodule_subdir.id();
                let mut new_submodule_commit_id = match mappings.get(&submodule_commit_id) {
                    Some(id) => *id,
                    None => submodule_commit_id,
                };
                new_submodule_commit_id = match old_id_to_new.get(&new_submodule_commit_id) {
                    Some(id) => *id,
                    None => {
                        let mapped = default_mapping.expect(&format!(
                            "Found a commit that isn't in mappings, \
                                              and default-mapping is empty: {}",
                            new_submodule_commit_id
                        ));
                        old_id_to_new[&mapped]
                    }
                };
                let submodule_commit = repo.find_commit(new_submodule_commit_id).expect(&format!(
                    "Couldn't obtain submodule's commit with ID {}",
                    new_submodule_commit_id
                ));
                let subtree_id = submodule_commit
                    .tree()
                    .and_then(|t| t.get_path(submodule_path))
                    .and_then(|te| Ok(te.id()))
                    .expect("Couldn't obtain submodule's subtree ID");

                let new_tree = replace_submodule_dir(&repo, &tree, &submodule_path, &subtree_id);

                // In commits that used to update the submodule, add a parent pointing to
                // appropriate commit in new submodule history
                let mut parent_subtree_ids = HashSet::new();
                for parent in commit.parents() {
                    let parent_tree = parent.tree().expect("Couldn't obtain parent's tree");
                    let parent_subdir_tree_id = parent_tree
                        .get_path(submodule_path)
                        .and_then(|x| Ok(x.id()));

                    match parent_subdir_tree_id {
                        Ok(id) => {
                            parent_subtree_ids.insert(id);
                            ()
                        }
                        Err(e) => {
                            if e.code() == git2::ErrorCode::NotFound
                                && e.class() == git2::ErrorClass::Tree
                            {
                                continue;
                            } else {
                                panic!("Error getting submodule's subdir from the tree: {:?}", e);
                            };
                        }
                    }
                }

                // Here's a few pictures to help you understand how we figure out if current commit
                // updated the submodule. If we draw a DAG and name submodule states, the following
                // situations will mean that the submodule wasn't updated:
                //
                //     o--o--o--A--
                //                 `,-A
                //      o--o--o--B-
                //
                // or
                //
                //     o--o--o--A--
                //                 `,-B
                //      o--o--o--B-
                //
                // And in the following graphs the submodule was updated:
                //
                //     o--o--o--A--
                //                 `,-C
                //      o--o--o--B-
                //
                // or
                //
                //     o--o--o--o--A--B
                //
                // Put into words, the rule will be "the submodule state in current commit is
                // different from states in all its parents". Or, more formally, the current state
                // doesn't belong to the set of states in parents.
                let submodule_updated: bool = !parent_subtree_ids.contains(&submodule_commit_id);

                // Rewrite the parents if the submodule was updated
                let parents = {
                    let mut p: Vec<Commit> = Vec::new();
                    for parent_id in commit.parent_ids() {
                        let actual_parent_id = old_id_to_new[&parent_id];
                        let parent = repo
                            .find_commit(actual_parent_id)
                            .expect("Couldn't find parent commit by its id");
                        p.push(parent);
                    }

                    if submodule_updated {
                        p.push(submodule_commit);
                    }

                    p
                };

                let mut parents_refs: Vec<&Commit> = Vec::new();
                for i in 0..parents.len() {
                    parents_refs.push(&parents[i]);
                }
                let new_commit_id = repo
                    .commit(
                        None,
                        &commit.author(),
                        &commit.committer(),
                        &commit
                            .message()
                            .expect("Couldn't retrieve commit's message"),
                        &new_tree,
                        &parents_refs[..],
                    )
                    .expect("Failed to commit");

                old_id_to_new.insert(oid, new_commit_id);
            }
            Err(e) => eprintln!("Error walking the repo's history: {:?}", e),
        }
    }

    let branches = repo
        .branches(Some(git2::BranchType::Local))
        .expect("Couldn't obtain an iterator over local branches");
    for maybe_branch in branches {
        match maybe_branch {
            Ok((branch, _)) => {
                let mut reference = branch.into_reference();
                let id = reference
                    .peel(git2::ObjectType::Commit)
                    .expect("Couldn't convert branch into a Commit")
                    .id();
                let new_id = old_id_to_new[&id];
                reference
                    .set_target(new_id, "git-submerge: moving to rewritten history")
                    .expect("Couldn't move branch to rewritten history");
            }
            Err(e) => eprintln!("Error walking the branches: {:?}", e),
        }
    }
}

fn replace_submodule_dir<'repo>(
    repo: &'repo Repository,
    tree: &Tree,
    submodule_path: &Path,
    subtree_id: &Oid,
) -> Tree<'repo> {
    let mut treebuilder = repo
        .treebuilder(Some(&tree))
        .expect("Couldn't create TreeBuilder");

    treebuilder
        .remove(submodule_path)
        .expect("Couldn't remove submodule path from TreeBuilder");
    treebuilder
        .insert(submodule_path, *subtree_id, 0o040000)
        .expect("Couldn't add submodule as a subdir to TreeBuilder");

    treebuilder
        .remove(".gitmodules")
        .expect("Couldn't remove .gitmodules from TreeBuilder");

    let new_tree_id = treebuilder
        .write()
        .expect("Couldn't write TreeBuilder into a Tree");
    let new_tree = repo
        .find_tree(new_tree_id)
        .expect("Couldn't read back the Tree we just wrote");

    new_tree
}

fn remove_dotgit_from_submodule(submodule_dir: &str) {
    let dotgit_path = String::from(submodule_dir) + "/.git";
    std::fs::remove_file(&dotgit_path).expect(&format!("Couldn't remove {}", dotgit_path));
}

fn remove_gitmodules() {
    let gitmodules_path = ".gitmodules";
    std::fs::remove_file(&gitmodules_path).expect("Couldn't remove .gitmodules");
}

fn update_index(repo: &Repository, old_id_to_new: &HashMap<Oid, Oid>) {
    let head = repo.head().expect("Couldn't obtain repo's HEAD");
    let head_id = head
        .target()
        .expect("Couldn't resolve repo's HEAD to a commit ID");
    let updated_id = match old_id_to_new.get(&head_id) {
        Some(id) => *id,
        // If the ID wasn't found, it's okay - it means it's one of the new ones. It means HEAD
        // was pointing at some branch, and since we've moved the branches at the end of repo's
        // history rewrite, HEAD doesn't need updating
        None => head_id,
    };
    let commit = repo
        .find_commit(updated_id)
        .expect("Coudln't get the commit HEAD points at");
    let tree = commit.tree().expect("Couldn't obtain commit's tree");
    let mut index = repo.index().expect("Couldn't obtain repo's index");
    index
        .read_tree(&tree)
        .expect("Couldn't populate the index with a tree");
    index
        .write()
        .expect("Couldn't write the index back to the repo");
}
