import shlex
from pathlib import Path

from harbor.agents.installed.base import with_prompt_template
from harbor.agents.installed.codex import Codex
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext
from harbor.models.trial.paths import EnvironmentPaths


class PersistentAuthCodex(Codex):
    """Harbor Codex agent with persisted auth.json refreshes.

    Harbor's stock Codex agent copies auth.json into each task container and
    deletes the container copy at the end. With Codex auth, a task may refresh
    the token in-container; dropping that refreshed file can make later tasks
    reuse an already-consumed refresh token. This wrapper keeps the stock Codex
    execution path but downloads the refreshed auth.json before cleanup.

    If `binary_path` is supplied, the wrapper uploads that Codex binary instead
    of installing the official npm package. That gives us a fair bare-patched
    baseline while keeping the run path otherwise equivalent to vanilla Codex.
    """

    @staticmethod
    def name() -> str:
        return "persistent-auth-codex"

    def __init__(self, *args, binary_path: str | None = None, **kwargs):
        super().__init__(*args, **kwargs)
        self._binary_path = Path(binary_path).expanduser() if binary_path else None
        if self._binary_path is not None and not self._binary_path.is_file():
            raise ValueError(f"binary_path does not exist: {self._binary_path}")

    async def install(self, environment: BaseEnvironment) -> None:
        if self._binary_path is None:
            await super().install(environment)
            return

        await self.exec_as_root(
            environment,
            command=(
                "if ldd --version 2>&1 | grep -qi musl || [ -f /etc/alpine-release ]; then"
                "  echo 'PersistentAuthCodex binary_path requires a glibc Linux task image' >&2; exit 1;"
                " elif command -v apt-get &>/dev/null; then"
                "  apt-get update &&"
                "  apt-get install -y --no-install-recommends "
                "curl ripgrep ca-certificates libzstd1 zlib1g &&"
                "  (apt-get install -y --no-install-recommends libssl3 ||"
                "   apt-get install -y --no-install-recommends libssl3t64);"
                " elif command -v yum &>/dev/null; then"
                "  yum install -y curl ripgrep ca-certificates;"
                " else"
                "  echo 'PersistentAuthCodex requires apt-get or a compatible glibc image' >&2; exit 1;"
                " fi"
            ),
            env={"DEBIAN_FRONTEND": "noninteractive"},
        )
        remote_codex = "/tmp/patched-codex"
        await environment.upload_file(self._binary_path, remote_codex)
        await self.exec_as_root(
            environment,
            command=(
                f"install -m 0755 {remote_codex} /usr/local/bin/codex && "
                "codex --version"
            ),
        )

    @with_prompt_template
    async def run(
        self, instruction: str, environment: BaseEnvironment, context: AgentContext
    ) -> None:
        escaped_instruction = shlex.quote(instruction)

        if not self.model_name:
            raise ValueError("Model name is required")

        model = self.model_name.split("/")[-1]
        cli_flags = self.build_cli_flags()
        cli_flags_arg = (cli_flags + " ") if cli_flags else ""

        auth_json_path = self._resolve_auth_json_path()
        remote_codex_home = self._REMOTE_CODEX_HOME.as_posix()
        remote_secrets_dir = self._REMOTE_CODEX_SECRETS_DIR.as_posix()
        remote_auth_path = (self._REMOTE_CODEX_SECRETS_DIR / "auth.json").as_posix()
        agent_dir = EnvironmentPaths.agent_dir.as_posix()

        env: dict[str, str] = {
            "CODEX_HOME": remote_codex_home,
            "NO_COLOR": "1",
            "TERM": "dumb",
        }

        await self.exec_as_agent(
            environment,
            command=(
                f'mkdir -p "$CODEX_HOME" {shlex.quote(remote_secrets_dir)} '
                f"{shlex.quote(agent_dir)}"
            ),
            env=env,
        )

        if auth_json_path:
            self.logger.debug("Codex auth: using auth.json from %s", auth_json_path)
            await environment.upload_file(auth_json_path, remote_auth_path)
            if environment.default_user is not None:
                await self.exec_as_root(
                    environment,
                    command=f"chown {environment.default_user} {remote_auth_path}",
                )
            setup_command = (
                f'ln -sf {shlex.quote(remote_auth_path)} "$CODEX_HOME/auth.json"\n'
            )
        else:
            self.logger.debug("Codex auth: using OPENAI_API_KEY")
            env["OPENAI_API_KEY"] = self._get_env("OPENAI_API_KEY") or ""
            setup_command = (
                f"cat >{shlex.quote(remote_auth_path)} <<EOF\n"
                '{\n  "OPENAI_API_KEY": "${OPENAI_API_KEY}"\n}\nEOF\n'
                f"ln -sf {shlex.quote(remote_auth_path)} "
                '"$CODEX_HOME/auth.json"\n'
            )

        if openai_base_url := self._get_env("OPENAI_BASE_URL"):
            env["OPENAI_BASE_URL"] = openai_base_url
            setup_command += (
                '\ncat >>"$CODEX_HOME/config.toml" <<TOML\n'
                'openai_base_url = "${OPENAI_BASE_URL}"\n'
                "TOML\n"
            )

        skills_command = self._build_register_skills_command()
        if skills_command:
            setup_command += f"\n{skills_command}"

        mcp_command = self._build_register_mcp_servers_command()
        if mcp_command:
            setup_command += f"\n{mcp_command}"

        if setup_command.strip():
            await self.exec_as_agent(environment, command=setup_command, env=env)

        try:
            await self.exec_as_agent(
                environment,
                command=(
                    "if [ -s ~/.nvm/nvm.sh ]; then . ~/.nvm/nvm.sh; fi; "
                    "codex exec "
                    "--dangerously-bypass-approvals-and-sandbox "
                    "--skip-git-repo-check "
                    f"--model {shlex.quote(model)} "
                    "--json "
                    "--enable unified_exec "
                    f"{cli_flags_arg}"
                    "-- "
                    f"{escaped_instruction} "
                    f"2>&1 </dev/null | tee {shlex.quote((EnvironmentPaths.agent_dir / self._OUTPUT_FILENAME).as_posix())}"
                ),
                env=env,
            )
        finally:
            try:
                await self.exec_as_agent(
                    environment,
                    command=(
                        f"mkdir -p {shlex.quote(agent_dir)}\n"
                        'if [ -d "$CODEX_HOME/sessions" ]; then\n'
                        f"  rm -rf {shlex.quote((EnvironmentPaths.agent_dir / 'sessions').as_posix())}\n"
                        f'  cp -R "$CODEX_HOME/sessions" {shlex.quote((EnvironmentPaths.agent_dir / "sessions").as_posix())}\n'
                        "fi\n"
                    ),
                    env=env,
                )
            except Exception:
                pass
            if auth_json_path:
                try:
                    await environment.download_file(remote_auth_path, auth_json_path)
                    auth_json_path.chmod(0o600)
                except Exception as exc:
                    self.logger.warning(
                        "Failed to persist refreshed Codex auth.json: %s", exc
                    )
            try:
                await self.exec_as_agent(
                    environment,
                    command=f'rm -rf {shlex.quote(remote_secrets_dir)} "$CODEX_HOME"',
                    env=env,
                )
            except Exception:
                pass
