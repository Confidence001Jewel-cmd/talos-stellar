"""CLI entry point — talos-agent start|config|status."""

from __future__ import annotations

import asyncio
from dataclasses import asdict
import json
import os
from pathlib import Path
import sys
import uuid

import click
from rich.console import Console
import re

from talos_agent import __version__
from talos_agent.config import APP_DIR, Settings, ensure_app_dir

console = Console()


@click.group()
@click.version_option(__version__, prog_name="talos-agent")
def main():
    """Talos Protocol Prime Agent — autonomous GTM agent."""


@main.command()
@click.option("--talos-id", default=None, help="Talos ID (overrides TALOS_ID in .env)")
@click.option("--env-file", default=".env", help="Path to .env file")
def start(talos_id: str | None, env_file: str):
    """Start the Prime Agent for a Talos."""
    ensure_app_dir()

    # Load .env into os.environ so child processes (Stagehand SEA) inherit them
    env_path = Path(env_file)
    if env_path.exists():
        from talos_agent.crypto import decrypt_with_password

        raw = env_path.read_text().splitlines()
        # detect whether any encrypted entries exist
        has_encrypted = any(
            "ENC::" in line
            for line in raw
            if line and "=" in line and not line.strip().startswith("#")
        )
        master_key = os.environ.get("TALOS_MASTER_KEY")
        if has_encrypted and not master_key:
            master_key = click.prompt("Master password (to decrypt secrets)", hide_input=True)

        for line in raw:
            line = line.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            key, _, value = line.partition("=")
            key = key.strip()
            value = value.strip()
            if value.startswith("ENC::"):
                try:
                    if not master_key:
                        console.print(f"[red]Error:[/red] Encrypted value for {key} but no master password available.")
                        sys.exit(1)
                    dec = decrypt_with_password(value, master_key)
                    os.environ.setdefault(key, dec)
                except Exception as e:
                    console.print(f"[red]Error decrypting {key}:[/red] {e}")
                    sys.exit(1)
            else:
                os.environ.setdefault(key, value)

    kwargs: dict = {"_env_file": env_file}
    if talos_id:
        kwargs["talos_id"] = talos_id
    settings = Settings(**kwargs)

    all_keys = settings.get_all_api_keys()
    if not all_keys:
        console.print("[red]Error:[/red] TALOS_API_KEY (or TALOS_API_KEYS) is required.")
        sys.exit(1)
    if not settings.llm_api_key:
        console.print("[red]Error:[/red] GROQ_API_KEY (or OPENAI_API_KEY) is required.")
        sys.exit(1)

    console.print(f"[bold green]Talos Agent v{__version__}[/bold green]")
    console.print(f"  Agents:    {len(all_keys)}")
    console.print(f"  API URL:   {settings.talos_api_url}")
    console.print()

    if len(all_keys) == 1:
        from talos_agent.scheduler import run
        asyncio.run(run(settings))
    else:
        from talos_agent.scheduler import run_multi
        asyncio.run(run_multi(settings, all_keys))


@main.command()
@click.option("--api-key", prompt="Talos API Key", help="API key issued at Talos creation")
@click.option("--openai-key", prompt="OpenAI API Key", help="OpenAI API key")
def config(api_key: str, openai_key: str):
    """Configure agent credentials (saved to ~/.talos-agent/config.json)."""
    if Settings().secret_rotation_enabled:
        raise click.ClickException(
            "plaintext config writes are disabled while secret rotation is enabled; "
            "use `talos-agent secrets rotate`"
        )
    ensure_app_dir()
    cfg_path = APP_DIR / "config.json"

    existing = {}
    if cfg_path.exists():
        existing = json.loads(cfg_path.read_text())

    existing.update({
        k: v for k, v in {
            "talos_api_key": api_key,
            "openai_api_key": openai_key,
        }.items() if v
    })

    cfg_path.write_text(json.dumps(existing, indent=2))
    console.print(f"[green]Config saved to {cfg_path}[/green]")



@main.command(name="encrypt-keys")
@click.option("--env-file", default=".env", help="Path to .env file to encrypt secrets in")
def encrypt_keys(env_file: str):
    """Encrypt plaintext secret-like values in an .env file using a master password."""
    from pathlib import Path
    from talos_agent.crypto import encrypt_with_password

    path = Path(env_file)
    if not path.exists():
        console.print(f"[red]Error:[/red] {path} not found")
        sys.exit(1)

    master_key = os.environ.get("TALOS_MASTER_KEY")
    if not master_key:
        master_key = click.prompt("Master password (to encrypt .env)", hide_input=True, confirmation_prompt=True)

    text = path.read_text()
    lines = text.splitlines()
    secret_re = re.compile(r"^S[A-Z2-7]{55}$")
    changed = 0
    out_lines = []
    for line in lines:
        raw = line
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            out_lines.append(raw)
            continue
        key, _, value = raw.partition("=")
        k = key.strip()
        v = value.strip()
        if v.startswith("ENC::"):
            out_lines.append(raw)
            continue
        if secret_re.match(v):
            enc = encrypt_with_password(v, master_key)
            out_lines.append(f"{k}={enc}")
            changed += 1
        else:
            out_lines.append(raw)

    if changed == 0:
        console.print("[yellow]No secret-like values found to encrypt.[/yellow]")
        return

    backup = path.with_suffix(path.suffix + ".bak") if path.suffix else Path(str(path) + ".bak")
    path.rename(backup)
    path.write_text("\n".join(out_lines) + "\n")
    console.print(f"[green]Encrypted {changed} values. Original saved to {backup}[/green]")


@main.command()
def status():
    """Show agent status."""
    from talos_agent.db import LocalDB

    ensure_app_dir()
    db = LocalDB()

    talos_cfg = db.get_talos_config()
    if talos_cfg:
        console.print(f"[bold]Talos:[/bold] {talos_cfg.get('name', 'Unknown')}")
    else:
        console.print("[yellow]No Talos config cached. Run `talos-agent start` first.[/yellow]")

    last_cycle = db.get_last_run("agent_cycle")
    if last_cycle:
        console.print(f"[bold]Last agent cycle:[/bold] {last_cycle.isoformat()}")

    posts_today = db.count_today("post")
    console.print(f"[bold]Posts today:[/bold] {posts_today}")

    playbook = db.get_active_playbook()
    if playbook:
        console.print(f"[bold]Active playbook:[/bold] {playbook['name']}")

    pending = db.get_pending_approvals()
    console.print(f"[bold]Pending approvals:[/bold] {len(pending)}")

    db.close()


@main.group()
@click.option(
    "--db-path",
    type=click.Path(path_type=Path),
    default=None,
    help="Secret database path (defaults to the prime-agent database)",
)
@click.pass_context
def secrets(ctx: click.Context, db_path: Path | None):
    """Manage encrypted, versioned runtime secrets."""
    from talos_agent.db import DB_PATH, LocalDB
    from talos_agent.secret_store import SecretStore, decode_keyring

    settings = Settings()
    try:
        db = LocalDB(
            path=db_path or DB_PATH,
            timeout_ms=settings.secret_db_timeout_ms,
        )
        store = SecretStore(
            db,
            keyring=decode_keyring(settings.secret_keyring),
            active_key_id=settings.secret_active_key_id,
            scope=settings.secret_scope,
            max_value_bytes=settings.secret_max_bytes,
            dual_read=settings.secret_dual_read,
            legacy_fallback=settings.secret_legacy_fallback,
        )
    except Exception as exc:
        raise click.ClickException(str(exc)) from exc
    ctx.obj = {"db": db, "store": store}
    ctx.call_on_close(db.close)


@secrets.command("stage")
@click.argument("name")
@click.option("--request-id", default=None, help="Idempotency key (generated if omitted)")
@click.option("--actor", default="operator", show_default=True)
@click.option("--reason", default=None, help="Lowercase audit reason code")
@click.pass_obj
def stage_secret(obj: dict, name: str, request_id: str | None, actor: str, reason: str | None):
    """Prompt for and stage an encrypted value without activating it."""
    value = click.prompt("Secret value", hide_input=True, confirmation_prompt=True)
    try:
        version = obj["store"].stage(
            name,
            value,
            request_id=request_id or str(uuid.uuid4()),
            actor=actor,
            reason=reason,
        )
    except Exception as exc:
        raise click.ClickException(str(exc)) from exc
    console.print_json(data=asdict(version))


@secrets.command("activate")
@click.argument("name")
@click.argument("version", type=click.IntRange(min=1))
@click.option("--expected-version", type=click.IntRange(min=1), default=None)
@click.option("--actor", default="operator", show_default=True)
@click.option("--reason", default=None, help="Lowercase audit reason code")
@click.pass_obj
def activate_secret(
    obj: dict,
    name: str,
    version: int,
    expected_version: int | None,
    actor: str,
    reason: str | None,
):
    """Atomically activate a staged version using compare-and-swap."""
    store = obj["store"]
    expected = expected_version if expected_version is not None else store.current_version(name)
    try:
        result = store.activate(
            name,
            version,
            expected_active_version=expected,
            actor=actor,
            reason=reason,
        )
    except Exception as exc:
        raise click.ClickException(str(exc)) from exc
    console.print_json(data=asdict(result))


@secrets.command("rotate")
@click.argument("name")
@click.option("--request-id", default=None, help="Idempotency key (generated if omitted)")
@click.option("--expected-version", type=click.IntRange(min=1), default=None)
@click.option("--actor", default="operator", show_default=True)
@click.option("--reason", default="routine_rotation", show_default=True)
@click.pass_obj
def rotate_secret(
    obj: dict,
    name: str,
    request_id: str | None,
    expected_version: int | None,
    actor: str,
    reason: str,
):
    """Prompt for, stage, and atomically activate a new encrypted version."""
    store = obj["store"]
    expected = expected_version if expected_version is not None else store.current_version(name)
    value = click.prompt("New secret value", hide_input=True, confirmation_prompt=True)
    try:
        staged = store.stage(
            name,
            value,
            request_id=request_id or str(uuid.uuid4()),
            actor=actor,
            reason=reason,
        )
        active = store.activate(
            name,
            staged.version,
            expected_active_version=expected,
            actor=actor,
            reason=reason,
        )
    except Exception as exc:
        raise click.ClickException(str(exc)) from exc
    console.print_json(data=asdict(active))


@secrets.command("recover")
@click.argument("name")
@click.argument("version", type=click.IntRange(min=1))
@click.option("--expected-version", type=click.IntRange(min=1), required=True)
@click.option("--actor", default="operator", show_default=True)
@click.option("--reason", default="credential_rejected", show_default=True)
@click.pass_obj
def recover_secret(
    obj: dict,
    name: str,
    version: int,
    expected_version: int,
    actor: str,
    reason: str,
):
    """Recover a prior non-revoked version with an atomic CAS."""
    try:
        result = obj["store"].recover(
            name,
            version,
            expected_active_version=expected_version,
            actor=actor,
            reason=reason,
        )
    except Exception as exc:
        raise click.ClickException(str(exc)) from exc
    console.print_json(data=asdict(result))


@secrets.command("revoke")
@click.argument("name")
@click.argument("version", type=click.IntRange(min=1))
@click.option("--actor", default="operator", show_default=True)
@click.option("--reason", default="rotation_complete", show_default=True)
@click.pass_obj
def revoke_secret(obj: dict, name: str, version: int, actor: str, reason: str):
    """Permanently exclude a non-active version from runtime reads."""
    try:
        result = obj["store"].revoke(name, version, actor=actor, reason=reason)
    except Exception as exc:
        raise click.ClickException(str(exc)) from exc
    console.print_json(data=asdict(result))


@secrets.command("list")
@click.argument("name")
@click.pass_obj
def list_secrets(obj: dict, name: str):
    """List lifecycle metadata. Ciphertext and key IDs are never displayed."""
    try:
        versions = obj["store"].list_versions(name)
    except Exception as exc:
        raise click.ClickException(str(exc)) from exc
    console.print_json(data=[asdict(version) for version in versions])


@secrets.command("audit")
@click.argument("name")
@click.option("--limit", type=click.IntRange(min=1, max=500), default=50)
@click.pass_obj
def audit_secrets(obj: dict, name: str, limit: int):
    """Show bounded secret lifecycle audit events."""
    try:
        events = obj["store"].audit_events(name, limit)
    except Exception as exc:
        raise click.ClickException(str(exc)) from exc
    console.print_json(data=events)
