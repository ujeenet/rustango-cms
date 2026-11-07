# rcms-scaffold

`rcms` creates a new [Rustango-CMS](https://github.com/ujeenet/rustango-cms) site: a runnable project with its page types, templates and a one-command local setup. Install it once:

```sh
cargo install rcms-scaffold
```

Then, from any folder:

```sh
rcms new mysite --theme blog
cd mysite
cargo run -- quickstart
```

`quickstart` creates a local SQLite database, a site called `demo` and an `admin` user, then starts the server. Open <http://demo.localhost:8080/> for the site and <http://demo.localhost:8080/cms-admin/> to edit it. The commands are the same in bash, zsh and Windows PowerShell.

## Options

| Option | What it does |
|---|---|
| `--theme blog\|news\|portfolio\|blog-portfolio` | A finished-looking site: page types, sample pages, menus, colours and error pages, all editable in the admin |
| `--template minimal\|blog` | A site with its page types as Rust models, to build your own (`minimal` is the default) |
| `--dir <folder>` | Where to create the project (default: the current folder) |
| `--local` | From a Rustango-CMS checkout: depend on that checkout instead of the crates.io release |

The scaffolder's version is the Rustango-CMS release it creates projects for; `rcms --version` shows it. Update it with `cargo install rcms-scaffold` again.

Guides: <https://cms.rustango.com>.

⭐ **Like it? Star [Rustango-CMS](https://github.com/ujeenet/rustango-cms) and [Rustango](https://github.com/ujeenet/rustango) on GitHub.** Stars help other Rust developers find both projects.
