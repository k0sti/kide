# kide-git

A Kide folder linked to one branch of a Git repository.

- `link` ties a folder to a branch. An empty folder takes the branch's files;
  a folder with files needs an empty or new branch.
- `status` lists what changed since the last commit both sides agree on.
- `commit` writes those changes as one Git commit on top of it; `push`
  publishes it without force.
- `pull` fetches the branch and takes its head into the folder as a root the
  folder pulls like another device's. Paths changed only upstream move
  forward; paths changed on both sides follow Kide's rule (prose merges,
  other files get conflict copies), and `commit` waits until no conflict
  copy is left.

The link lives in the folder's `.kide/git/`, which never syncs: a bare
repository, the settings and a map from commits to Kide roots. Git runs as
the installed `git` program, with the repository's hooks disabled; Git LFS
and submodules are refused. The `kide git` commands of `kide-cli` use it.
