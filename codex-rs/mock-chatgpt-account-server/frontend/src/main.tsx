import { FormEvent, ReactNode, StrictMode, useEffect, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import "./styles.css";

type LoginBootstrap = {
  page: "browserLogin";
  continueTo: string;
  usernameHint: string;
  passwordHint: string;
  errorMessage: string | null;
  socialProviders: SocialLoginProvider[];
};

type DeviceRecord = {
  userCode: string;
  approved: boolean;
  polls: number;
};

type SocialLoginProvider = {
  id: string;
  label: string;
  subtitle: string | null;
};

type DeviceBootstrap = {
  page: "deviceAuth";
  records: DeviceRecord[];
  message: string | null;
};

type AccountConfirmBootstrap = {
  page: "accountConfirm";
  continueTo: string;
  email: string;
  accountId: string;
  planType: string;
  organizationId: string;
  projectId: string;
  redirectUri: string;
  oauthState: string;
};

type TaskData = {
  taskId: string;
  title: string;
  userPrompt: string;
  assistantResponse: string;
};

type TaskBootstrap = {
  page: "taskView";
  task: TaskData | null;
  missingTaskId: string | null;
};

type CallbackBootstrap = {
  page: "callback";
};

type RemoteControlBootstrap = {
  page: "remoteControl";
  backendBaseUrl: string;
  bearerToken: string;
  accountId: string;
  suggestedInstallationId: string;
  suggestedServerName: string;
  strictAccountHeader: boolean;
  protocolVersion: string;
};

type Bootstrap =
  | LoginBootstrap
  | AccountConfirmBootstrap
  | DeviceBootstrap
  | TaskBootstrap
  | RemoteControlBootstrap
  | CallbackBootstrap;

type LoginActionResponse = {
  ok: boolean;
  redirectTo?: string;
  errorMessage?: string;
};

type LoginStep = "email" | "password";

function readBootstrap(): Bootstrap {
  const script = document.getElementById("mock-bootstrap");
  if (!script?.textContent) {
    throw new Error("missing bootstrap payload");
  }
  return JSON.parse(script.textContent) as Bootstrap;
}

function Shell(props: { title: string; eyebrow: string; children: ReactNode }) {
  return (
    <main className="page-shell">
      <section className="hero-panel">
        <p className="eyebrow">{props.eyebrow}</p>
        <h1>{props.title}</h1>
        {props.children}
      </section>
    </main>
  );
}

function AuthFooterLinks() {
  function preventDefault(event: React.MouseEvent<HTMLAnchorElement>) {
    event.preventDefault();
  }

  return (
    <div className="auth-footer-links">
      <a href="#" onClick={preventDefault}>
        使用条款
      </a>
      <span>|</span>
      <a href="#" onClick={preventDefault}>
        隐私政策
      </a>
    </div>
  );
}

function AuthGlyph() {
  return (
    <div className="auth-glyph">
      <svg aria-hidden="true" fill="none" viewBox="0 0 40 40">
        <circle cx="20" cy="20" r="12" stroke="currentColor" strokeWidth="2.6" />
        <path d="M14 24h8" stroke="currentColor" strokeLinecap="round" strokeWidth="2.6" />
        <path d="M18 14a5 5 0 0 1 6 6" stroke="currentColor" strokeLinecap="round" strokeWidth="2.6" />
        <path d="M12.5 18.5 10.8 20l1.7 1.5" stroke="currentColor" strokeLinecap="round" strokeLinejoin="round" strokeWidth="2.6" />
      </svg>
    </div>
  );
}

function providerDisplayName(provider: SocialLoginProvider) {
  if (provider.id === "google") {
    return "Google";
  }
  if (provider.id === "github") {
    return "GitHub";
  }
  return provider.label;
}

function ProviderIcon(props: { providerId: string }) {
  if (props.providerId === "google") {
    return <span className="provider-icon provider-icon-google">G</span>;
  }
  if (props.providerId === "github") {
    return <span className="provider-icon provider-icon-github">GH</span>;
  }
  return <span className="provider-icon">{props.providerId.slice(0, 2).toUpperCase()}</span>;
}

function BrowserLoginPage(props: { bootstrap: LoginBootstrap }) {
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [errorMessage, setErrorMessage] = useState(props.bootstrap.errorMessage);
  const [submitting, setSubmitting] = useState(false);
  const [shortcutSubmitting, setShortcutSubmitting] = useState<string | null>(null);
  const [step, setStep] = useState<LoginStep>("email");

  useEffect(() => {
    document.title = "欢迎回来";
  }, []);

  function handleEmailContinue(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!email.trim()) {
      setErrorMessage("请输入电子邮件地址。");
      return;
    }
    setErrorMessage(null);
    setStep("password");
  }

  async function handlePasswordSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!password) {
      setErrorMessage("请输入密码。");
      return;
    }
    setSubmitting(true);
    setErrorMessage(null);

    const response = await fetch("/oauth/login", {
      method: "POST",
      headers: {
        Accept: "application/json",
        "Content-Type": "application/x-www-form-urlencoded",
      },
      body: new URLSearchParams({
        username: email.trim(),
        password,
        continue_to: props.bootstrap.continueTo,
      }).toString(),
      credentials: "same-origin",
    });
    const payload = (await response.json()) as LoginActionResponse;
    setSubmitting(false);

    if (!payload.ok) {
      setErrorMessage(payload.errorMessage ?? "登录失败，请检查邮箱和密码。");
      return;
    }

    window.location.assign(payload.redirectTo ?? props.bootstrap.continueTo);
  }

  async function handleShortcutSubmit(event: FormEvent<HTMLFormElement>, providerId: string) {
    event.preventDefault();
    setShortcutSubmitting(providerId);
    setErrorMessage(null);

    const response = await fetch("/oauth/login/shortcut", {
      method: "POST",
      headers: {
        Accept: "application/json",
        "Content-Type": "application/x-www-form-urlencoded",
      },
      body: new URLSearchParams({
        provider: providerId,
        continue_to: props.bootstrap.continueTo,
      }).toString(),
      credentials: "same-origin",
    });
    const payload = (await response.json()) as LoginActionResponse;
    setShortcutSubmitting(null);

    if (!payload.ok) {
      setErrorMessage(payload.errorMessage ?? "快捷登录失败。");
      return;
    }

    window.location.assign(payload.redirectTo ?? props.bootstrap.continueTo);
  }

  return (
    <main className="auth-page">
      <section className="login-shell">
        <h1 className="login-title">欢迎回来</h1>
        {errorMessage ? <p className="auth-error">{errorMessage}</p> : null}
        {step === "email" ? (
          <form action="/oauth/login" className="login-form" method="post" onSubmit={handleEmailContinue}>
            <input
              autoComplete="username"
              className="auth-input"
              name="username"
              placeholder="电子邮件地址"
              value={email}
              onChange={(event) => setEmail(event.target.value)}
            />
            <button className="auth-primary-button" disabled={shortcutSubmitting !== null} type="submit">
              继续
            </button>
          </form>
        ) : (
          <form action="/oauth/login" className="login-form" method="post" onSubmit={handlePasswordSubmit}>
            <div className="identity-pill-row">
              <button
                className="identity-pill"
                type="button"
                onClick={() => {
                  setStep("email");
                  setPassword("");
                  setErrorMessage(null);
                }}
              >
                <span className="identity-pill-icon">@</span>
                <span>{email.trim() || props.bootstrap.usernameHint}</span>
              </button>
            </div>
            <input name="continue_to" type="hidden" value={props.bootstrap.continueTo} />
            <input name="username" type="hidden" value={email.trim()} />
            <input
              autoComplete="current-password"
              className="auth-input"
              name="password"
              placeholder="密码"
              type="password"
              value={password}
              onChange={(event) => setPassword(event.target.value)}
            />
            <button className="auth-primary-button" disabled={submitting} type="submit">
              {submitting ? "继续中..." : "继续"}
            </button>
          </form>
        )}
        <p className="register-line">
          还没有账户？
          <a href="#" onClick={(event) => event.preventDefault()}>
            请注册
          </a>
        </p>
        {props.bootstrap.socialProviders.length > 0 ? (
          <>
            <div className="divider-line">
              <span>或</span>
            </div>
            <div className="provider-list">
              {props.bootstrap.socialProviders.map((provider) => (
                <form
                  action="/oauth/login/shortcut"
                  className="provider-form"
                  key={provider.id}
                  method="post"
                  onSubmit={(event) => handleShortcutSubmit(event, provider.id)}
                >
                  <input name="provider" type="hidden" value={provider.id} />
                  <input name="continue_to" type="hidden" value={props.bootstrap.continueTo} />
                  <button className="provider-button" disabled={shortcutSubmitting !== null} type="submit">
                    <span className="provider-button-main">
                      <ProviderIcon providerId={provider.id} />
                      <span>{`继续使用 ${providerDisplayName(provider)} 登录`}</span>
                    </span>
                    {shortcutSubmitting === provider.id ? <span className="provider-side-text">连接中</span> : null}
                  </button>
                </form>
              ))}
            </div>
          </>
        ) : null}
        <AuthFooterLinks />
      </section>
    </main>
  );
}

function AccountConfirmPage(props: { bootstrap: AccountConfirmBootstrap }) {
  const [submitting, setSubmitting] = useState(false);
  const workspaceOptions =
    props.bootstrap.organizationId && props.bootstrap.organizationId !== "personal"
      ? [
          {
            id: "workspace",
            name: props.bootstrap.organizationId,
            subtitle: `${props.bootstrap.planType.toUpperCase()} · 团队空间`,
            mark: props.bootstrap.organizationId.slice(0, 2).toLowerCase(),
            tone: "workspace" as const,
          },
          {
            id: "personal",
            name: "个人账户",
            subtitle: props.bootstrap.accountId,
            mark: props.bootstrap.email.slice(0, 1).toLowerCase(),
            tone: "personal" as const,
          },
        ]
      : [
          {
            id: "personal",
            name: "个人账户",
            subtitle: props.bootstrap.accountId,
            mark: props.bootstrap.email.slice(0, 1).toLowerCase(),
            tone: "personal" as const,
          },
        ];
  const [selectedWorkspace, setSelectedWorkspace] = useState(workspaceOptions[0]?.id ?? "personal");

  useEffect(() => {
    document.title = "使用 ChatGPT 登录到 Codex";
  }, []);

  async function handleSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setSubmitting(true);

    const response = await fetch("/oauth/authorize/approve", {
      method: "POST",
      headers: {
        Accept: "application/json",
        "Content-Type": "application/x-www-form-urlencoded",
      },
      body: new URLSearchParams({
        continue_to: props.bootstrap.continueTo,
      }).toString(),
      credentials: "same-origin",
    });
    const payload = (await response.json()) as LoginActionResponse;
    setSubmitting(false);

    if (!payload.ok) {
      window.location.assign(props.bootstrap.continueTo);
      return;
    }

    window.location.assign(payload.redirectTo ?? props.bootstrap.redirectUri);
  }

  return (
    <main className="auth-page">
      <section className="confirm-shell">
        <AuthGlyph />
        <h1 className="confirm-title">使用 ChatGPT 登录到 Codex</h1>
        <div className="identity-pill-row">
          <div className="identity-pill static">
            <span className="identity-pill-icon">@</span>
            <span>{props.bootstrap.email}</span>
          </div>
        </div>
        <section className="workspace-section">
          <h2 className="workspace-heading">选择一个工作空间</h2>
          <div className="workspace-list">
            {workspaceOptions.map((workspace) => {
              const selected = selectedWorkspace === workspace.id;
              return (
                <button
                  className={`workspace-option ${selected ? "selected" : ""}`}
                  key={workspace.id}
                  type="button"
                  onClick={() => setSelectedWorkspace(workspace.id)}
                >
                  <span className={`workspace-mark ${workspace.tone}`}>
                    {workspace.mark}
                  </span>
                  <span className="workspace-copy">
                    <strong>{workspace.name}</strong>
                    <span>{workspace.subtitle}</span>
                  </span>
                  <span className={`workspace-check ${selected ? "visible" : ""}`}>✓</span>
                </button>
              );
            })}
          </div>
        </section>
        <div className="confirm-copy">
          <p>继续操作后，ChatGPT 将向 Codex 提供你的姓名、电子邮件和个人资料头像以关联你的帐户。</p>
          <p>Codex 不会收到你的聊天历史记录。</p>
          <p>
            在你使用 Codex 时：
            <br />
            该功能由你的 ChatGPT 帐户提供支持，并使用你当前套餐的速率限制、训练及语言偏好设置。
          </p>
          <p>
            ChatGPT 使用条款和隐私政策（或适用于 ChatGPT Enterprise、Education 或 Business 用户的对应服务条款）适用于与 ChatGPT
            共享的数据。
          </p>
          <p>Codex 可能存在错误。请务必审查其编写的代码和执行的命令。</p>
        </div>
        <div className="confirm-actions">
          <a
            className="auth-secondary-button"
            href={`/oauth/logout?continue_to=${encodeURIComponent(props.bootstrap.continueTo)}`}
          >
            取消
          </a>
          <form action="/oauth/authorize/approve" method="post" onSubmit={handleSubmit}>
            <input name="continue_to" type="hidden" value={props.bootstrap.continueTo} />
            <button className="auth-primary-button" disabled={submitting} type="submit">
              {submitting ? "继续中..." : "继续"}
            </button>
          </form>
        </div>
        <AuthFooterLinks />
      </section>
    </main>
  );
}

function DeviceAuthPage(props: { bootstrap: DeviceBootstrap }) {
  const [records, setRecords] = useState(props.bootstrap.records);
  const [message, setMessage] = useState(props.bootstrap.message);
  const [userCode, setUserCode] = useState("");
  const [submitting, setSubmitting] = useState(false);

  useEffect(() => {
    document.title = "Mock Device Auth";
    const interval = window.setInterval(async () => {
      const response = await fetch("/codex/device", {
        headers: { Accept: "application/json" },
        credentials: "same-origin",
      });
      const payload = (await response.json()) as DeviceBootstrap;
      setRecords(payload.records);
      setMessage((current) => current ?? payload.message);
    }, 2000);
    return () => window.clearInterval(interval);
  }, []);

  async function handleSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setSubmitting(true);

    const response = await fetch("/codex/device", {
      method: "POST",
      headers: {
        Accept: "application/json",
        "Content-Type": "application/x-www-form-urlencoded",
      },
      body: new URLSearchParams({ user_code: userCode }).toString(),
      credentials: "same-origin",
    });
    const payload = (await response.json()) as DeviceBootstrap;
    setRecords(payload.records);
    setMessage(payload.message);
    setSubmitting(false);
    setUserCode("");
  }

  return (
    <Shell title="Device Authorization Console" eyebrow="Device Code Flow">
      <p className="lead">
        Open this page after <code>codex login --device-auth</code> prints a user code. The board refreshes automatically.
      </p>
      {message ? <p className="flash">{message}</p> : null}
      <form action="/codex/device" className="inline-form" method="post" onSubmit={handleSubmit}>
        <input
          name="user_code"
          placeholder="Enter the printed user code"
          value={userCode}
          onChange={(event) => setUserCode(event.target.value)}
        />
        <button className="primary-button" disabled={submitting} type="submit">
          {submitting ? "Approving..." : "Approve"}
        </button>
      </form>
      <div className="table-card">
        <div className="table-title">
          <strong>Active device codes</strong>
          <span>Polling every 2s</span>
        </div>
        <table>
          <thead>
            <tr>
              <th>User code</th>
              <th>Approved</th>
              <th>Polls</th>
            </tr>
          </thead>
          <tbody>
            {records.length === 0 ? (
              <tr>
                <td colSpan={3}>No active device codes yet.</td>
              </tr>
            ) : (
              records.map((record) => (
                <tr key={record.userCode}>
                  <td>
                    <code>{record.userCode}</code>
                  </td>
                  <td>{record.approved ? "yes" : "no"}</td>
                  <td>{record.polls}</td>
                </tr>
              ))
            )}
          </tbody>
        </table>
      </div>
    </Shell>
  );
}

function TaskPage(props: { bootstrap: TaskBootstrap }) {
  useEffect(() => {
    document.title = props.bootstrap.task?.title ?? "Unknown task";
  }, [props.bootstrap.task]);

  if (!props.bootstrap.task) {
    return (
      <Shell title="Unknown task" eyebrow="Task View">
        <p className="lead">
          The mock server does not know task <code>{props.bootstrap.missingTaskId}</code>.
        </p>
      </Shell>
    );
  }

  return (
    <Shell title={props.bootstrap.task.title} eyebrow="Task View">
      <p className="lead">
        Task ID <code>{props.bootstrap.task.taskId}</code>
      </p>
      <div className="content-grid">
        <section className="content-card">
          <h2>User prompt</h2>
          <pre>{props.bootstrap.task.userPrompt}</pre>
        </section>
        <section className="content-card">
          <h2>Assistant response</h2>
          <pre>{props.bootstrap.task.assistantResponse}</pre>
        </section>
      </div>
    </Shell>
  );
}

function CallbackPage() {
  useEffect(() => {
    document.title = "Mock Device Callback";
  }, []);

  return (
    <Shell title="Mock device callback reached" eyebrow="Device Callback">
      <p className="lead">The local device-auth flow redirected back successfully.</p>
    </Shell>
  );
}

// -----------------------------------------------------------------------------
// Remote-control console
// -----------------------------------------------------------------------------

type LogDirection = "info" | "phone-out" | "phone-in" | "codex-out" | "codex-in" | "error";

type LogEntry = {
  id: number;
  ts: string;
  direction: LogDirection;
  text: string;
};

type EnrollResult = {
  serverId: string;
  environmentId: string;
};

type LinkStatus = "idle" | "connecting" | "open" | "closing" | "closed" | "error";

function nowStamp() {
  const d = new Date();
  const hh = d.getHours().toString().padStart(2, "0");
  const mm = d.getMinutes().toString().padStart(2, "0");
  const ss = d.getSeconds().toString().padStart(2, "0");
  const ms = d.getMilliseconds().toString().padStart(3, "0");
  return `${hh}:${mm}:${ss}.${ms}`;
}

function newClientId() {
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
    return `client-${crypto.randomUUID()}`;
  }
  return `client-${Math.random().toString(36).slice(2, 10)}`;
}

function newStreamId() {
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
    return `stream-${crypto.randomUUID()}`;
  }
  return `stream-${Math.random().toString(36).slice(2, 10)}`;
}

function statusLabel(status: LinkStatus) {
  switch (status) {
    case "idle":
      return "未连接";
    case "connecting":
      return "正在连接...";
    case "open":
      return "已连接";
    case "closing":
      return "正在断开...";
    case "closed":
      return "已断开";
    case "error":
      return "错误";
  }
}

function toWsUrl(baseHttpUrl: string, path: string) {
  const url = new URL(path, baseHttpUrl.endsWith("/") ? baseHttpUrl : `${baseHttpUrl}/`);
  if (url.protocol === "https:") url.protocol = "wss:";
  else if (url.protocol === "http:") url.protocol = "ws:";
  return url.toString();
}

function RemoteControlPage(props: { bootstrap: RemoteControlBootstrap }) {
  const { bootstrap } = props;

  // Enrollment form state.
  const [bearer, setBearer] = useState(bootstrap.bearerToken);
  const [accountId, setAccountId] = useState(bootstrap.accountId);
  const [installationId, setInstallationId] = useState(bootstrap.suggestedInstallationId);
  const [serverName, setServerName] = useState(bootstrap.suggestedServerName);
  const [enrolled, setEnrolled] = useState<EnrollResult | null>(null);
  const [enrollError, setEnrollError] = useState<string | null>(null);
  const [enrolling, setEnrolling] = useState(false);

  // Session state.
  const [clientId, setClientId] = useState(newClientId);
  const [streamId, setStreamId] = useState(newStreamId);
  const [phoneStatus, setPhoneStatus] = useState<LinkStatus>("idle");
  const [phoneCloseCode, setPhoneCloseCode] = useState<number | null>(null);
  const phoneRef = useRef<WebSocket | null>(null);
  const phoneSubscribeCursorRef = useRef<string | null>(null);
  const phoneHighestSeqRef = useRef<number | null>(null);

  // Codex-side WS (optional — lets the user simulate both ends in dev).
  const [codexStatus, setCodexStatus] = useState<LinkStatus>("idle");
  const [codexCloseCode, setCodexCloseCode] = useState<number | null>(null);
  const codexRef = useRef<WebSocket | null>(null);
  const codexSubscribeCursorRef = useRef<string | null>(null);

  // Log + composer.
  const [log, setLog] = useState<LogEntry[]>([]);
  const logSeqRef = useRef(0);
  const [phoneDraft, setPhoneDraft] = useState(
    JSON.stringify(
      {
        type: "client_message",
        message: { jsonrpc: "2.0", method: "ping", id: 1 },
      },
      null,
      2,
    ),
  );
  const [codexDraft, setCodexDraft] = useState(
    JSON.stringify(
      {
        type: "server_message",
        message: { jsonrpc: "2.0", result: "hello from codex", id: 1 },
        seq_id: 1,
      },
      null,
      2,
    ),
  );

  useEffect(() => {
    document.title = "Remote Control Console";
  }, []);

  // Tear down sockets on unmount so navigating away doesn't leak them.
  useEffect(() => {
    return () => {
      phoneRef.current?.close();
      codexRef.current?.close();
    };
  }, []);

  function append(direction: LogDirection, text: string) {
    logSeqRef.current += 1;
    setLog((prev) =>
      [
        ...prev,
        { id: logSeqRef.current, ts: nowStamp(), direction, text } satisfies LogEntry,
      ].slice(-500),
    );
  }

  function clearLog() {
    setLog([]);
  }

  async function handleEnroll(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setEnrolling(true);
    setEnrollError(null);
    try {
      const url = `${bootstrap.backendBaseUrl}/wham/remote/control/server/enroll`;
      const response = await fetch(url, {
        method: "POST",
        headers: {
          Authorization: `Bearer ${bearer}`,
          "chatgpt-account-id": accountId,
          "x-codex-installation-id": installationId,
          "Content-Type": "application/json",
          Accept: "application/json",
        },
        body: JSON.stringify({
          name: serverName,
          os: "browser",
          arch: navigator.platform || "unknown",
          app_server_version: "console",
          installation_id: installationId,
        }),
      });
      const text = await response.text();
      if (!response.ok) {
        setEnrollError(`HTTP ${response.status}: ${text || response.statusText}`);
        append("error", `enroll failed: HTTP ${response.status} ${text}`);
        return;
      }
      const payload = JSON.parse(text) as { server_id: string; environment_id: string };
      setEnrolled({ serverId: payload.server_id, environmentId: payload.environment_id });
      append(
        "info",
        `enroll OK — server_id=${payload.server_id} environment_id=${payload.environment_id}`,
      );
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      setEnrollError(msg);
      append("error", `enroll error: ${msg}`);
    } finally {
      setEnrolling(false);
    }
  }

  function regenerateInstallationId() {
    setInstallationId(`install-${Math.random().toString(36).slice(2, 12)}`);
    setEnrolled(null);
  }

  function regenerateClientId() {
    setClientId(newClientId());
  }
  function regenerateStreamId() {
    setStreamId(newStreamId());
  }

  function noteCursorFromEnvelope(envelope: unknown, side: "phone" | "codex") {
    if (typeof envelope !== "object" || envelope === null) return;
    const value = (envelope as Record<string, unknown>).cursor;
    if (typeof value === "string" && value.length > 0) {
      if (side === "phone") phoneSubscribeCursorRef.current = value;
      else codexSubscribeCursorRef.current = value;
    }
    const seqId = (envelope as Record<string, unknown>).seq_id;
    if (side === "phone" && typeof seqId === "number") {
      if (phoneHighestSeqRef.current === null || seqId > phoneHighestSeqRef.current) {
        phoneHighestSeqRef.current = seqId;
      }
    }
  }

  function connectPhone() {
    if (!enrolled) return;
    if (phoneRef.current && phoneRef.current.readyState <= WebSocket.OPEN) return;

    const url = new URL(
      "wham/remote/control/client",
      bootstrap.backendBaseUrl.endsWith("/")
        ? bootstrap.backendBaseUrl
        : `${bootstrap.backendBaseUrl}/`,
    );
    url.searchParams.set("environment_id", enrolled.environmentId);
    url.searchParams.set("client_id", clientId);
    url.searchParams.set("stream_id", streamId);
    if (phoneSubscribeCursorRef.current) {
      url.searchParams.set("subscribe_cursor", phoneSubscribeCursorRef.current);
    }
    if (url.protocol === "https:") url.protocol = "wss:";
    else if (url.protocol === "http:") url.protocol = "ws:";

    // Browsers don't let us set Authorization / chatgpt-account-id headers on
    // WebSocket handshakes. The Sec-WebSocket-Protocol negotiation is the
    // canonical workaround the relay supports — but the mock currently
    // requires those headers. As a dev convenience, forward them via query
    // params; the relay accepts both shapes for the console use case.
    url.searchParams.set("bearer", bearer);
    url.searchParams.set("chatgpt-account-id", accountId);

    append("info", `phone WSS → ${url.toString()}`);
    setPhoneStatus("connecting");
    setPhoneCloseCode(null);

    let ws: WebSocket;
    try {
      ws = new WebSocket(url.toString());
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      append("error", `phone WSS error: ${msg}`);
      setPhoneStatus("error");
      return;
    }
    phoneRef.current = ws;
    ws.onopen = () => setPhoneStatus("open");
    ws.onerror = () => {
      append("error", "phone WSS error event");
    };
    ws.onclose = (ev) => {
      setPhoneStatus("closed");
      setPhoneCloseCode(ev.code);
      append("info", `phone WSS closed code=${ev.code} reason=${ev.reason || "(none)"}`);
      if (phoneRef.current === ws) phoneRef.current = null;
    };
    ws.onmessage = (ev) => {
      const text = typeof ev.data === "string" ? ev.data : "(binary)";
      append("phone-in", text);
      try {
        noteCursorFromEnvelope(JSON.parse(text), "phone");
      } catch {
        // Non-JSON; relay sometimes emits raw close frames. Ignore.
      }
    };
  }

  function disconnectPhone() {
    if (phoneRef.current) {
      setPhoneStatus("closing");
      phoneRef.current.close();
    }
  }

  function connectCodex() {
    if (!enrolled) return;
    if (codexRef.current && codexRef.current.readyState <= WebSocket.OPEN) return;

    const url = new URL(
      "wham/remote/control/server",
      bootstrap.backendBaseUrl.endsWith("/")
        ? bootstrap.backendBaseUrl
        : `${bootstrap.backendBaseUrl}/`,
    );
    if (url.protocol === "https:") url.protocol = "wss:";
    else if (url.protocol === "http:") url.protocol = "ws:";
    if (codexSubscribeCursorRef.current) {
      url.searchParams.set("subscribe_cursor", codexSubscribeCursorRef.current);
    }
    url.searchParams.set("bearer", bearer);
    url.searchParams.set("chatgpt-account-id", accountId);
    url.searchParams.set("x-codex-installation-id", installationId);
    url.searchParams.set("x-codex-server-id", enrolled.serverId);
    url.searchParams.set("x-codex-protocol-version", bootstrap.protocolVersion);

    append("info", `codex WSS → ${url.toString()}`);
    setCodexStatus("connecting");
    setCodexCloseCode(null);

    let ws: WebSocket;
    try {
      ws = new WebSocket(url.toString());
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      append("error", `codex WSS error: ${msg}`);
      setCodexStatus("error");
      return;
    }
    codexRef.current = ws;
    ws.onopen = () => setCodexStatus("open");
    ws.onerror = () => append("error", "codex WSS error event");
    ws.onclose = (ev) => {
      setCodexStatus("closed");
      setCodexCloseCode(ev.code);
      append("info", `codex WSS closed code=${ev.code} reason=${ev.reason || "(none)"}`);
      if (codexRef.current === ws) codexRef.current = null;
    };
    ws.onmessage = (ev) => {
      const text = typeof ev.data === "string" ? ev.data : "(binary)";
      append("codex-in", text);
      try {
        noteCursorFromEnvelope(JSON.parse(text), "codex");
      } catch {
        // ignore parse errors
      }
    };
  }

  function disconnectCodex() {
    if (codexRef.current) {
      setCodexStatus("closing");
      codexRef.current.close();
    }
  }

  function sendFromPhone() {
    const ws = phoneRef.current;
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      append("error", "phone not connected");
      return;
    }
    const text = buildEnvelopeFromDraft(phoneDraft, clientId, streamId, "phone");
    if (text === null) return;
    ws.send(text);
    append("phone-out", text);
  }

  function sendFromCodex() {
    const ws = codexRef.current;
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      append("error", "codex not connected");
      return;
    }
    const text = buildEnvelopeFromDraft(codexDraft, clientId, streamId, "codex");
    if (text === null) return;
    ws.send(text);
    append("codex-out", text);
  }

  function buildEnvelopeFromDraft(
    draft: string,
    cId: string,
    sId: string,
    side: "phone" | "codex",
  ): string | null {
    let parsed: Record<string, unknown>;
    try {
      const value = JSON.parse(draft);
      if (typeof value !== "object" || value === null || Array.isArray(value)) {
        throw new Error("envelope must be a JSON object");
      }
      parsed = value as Record<string, unknown>;
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      append("error", `invalid JSON: ${msg}`);
      return null;
    }
    if (typeof parsed.client_id !== "string") parsed.client_id = cId;
    if (typeof parsed.stream_id !== "string") parsed.stream_id = sId;
    if (side === "codex" && typeof parsed.seq_id !== "number") {
      parsed.seq_id = 1;
    }
    return JSON.stringify(parsed);
  }

  function sendAckFromPhone() {
    if (!phoneRef.current || phoneRef.current.readyState !== WebSocket.OPEN) {
      append("error", "phone not connected");
      return;
    }
    const highest = phoneHighestSeqRef.current;
    if (highest === null) {
      append("error", "no seq_id seen yet — cannot ack");
      return;
    }
    const payload = {
      type: "ack",
      client_id: clientId,
      stream_id: streamId,
      seq_id: highest,
    };
    const text = JSON.stringify(payload);
    phoneRef.current.send(text);
    append("phone-out", text);
  }

  function sendClientClosedFromPhone(scope: "stream" | "client") {
    if (!phoneRef.current || phoneRef.current.readyState !== WebSocket.OPEN) {
      append("error", "phone not connected");
      return;
    }
    const payload: Record<string, unknown> = {
      type: "client_closed",
      client_id: clientId,
    };
    if (scope === "stream") {
      payload.stream_id = streamId;
    }
    const text = JSON.stringify(payload);
    phoneRef.current.send(text);
    append("phone-out", text);
  }

  const phoneCanConnect = enrolled !== null && phoneStatus !== "open" && phoneStatus !== "connecting";
  const phoneCanDisconnect = phoneStatus === "open" || phoneStatus === "connecting";
  const codexCanConnect = enrolled !== null && codexStatus !== "open" && codexStatus !== "connecting";
  const codexCanDisconnect = codexStatus === "open" || codexStatus === "connecting";

  return (
    <main className="page-shell">
      <section className="hero-panel">
        <p className="eyebrow">Remote Control Console</p>
        <h1>wham/remote/control 调试控制台</h1>
        <p className="lead">
          注册 Codex 远控 server，并在浏览器里同时驱动 Phone / Codex 两端的 WebSocket，用来端到端验证
          mock relay 的行为（enroll、cursor 重放、ack 裁剪、ClientClosed 清理等）。
        </p>

        <section className="content-card">
          <h2>1. Enroll</h2>
          <form className="inline-form" onSubmit={handleEnroll} style={{ flexWrap: "wrap" }}>
            <label>
              <span style={{ display: "block", fontSize: 12, color: "var(--muted)" }}>Bearer token</span>
              <input
                onChange={(e) => setBearer(e.target.value)}
                placeholder="access token"
                style={{ minWidth: 260 }}
                value={bearer}
              />
            </label>
            <label>
              <span style={{ display: "block", fontSize: 12, color: "var(--muted)" }}>chatgpt-account-id</span>
              <input
                onChange={(e) => setAccountId(e.target.value)}
                placeholder="org-debug"
                style={{ minWidth: 200 }}
                value={accountId}
              />
            </label>
            <label>
              <span style={{ display: "block", fontSize: 12, color: "var(--muted)" }}>installation_id</span>
              <input
                onChange={(e) => setInstallationId(e.target.value)}
                style={{ minWidth: 260 }}
                value={installationId}
              />
            </label>
            <label>
              <span style={{ display: "block", fontSize: 12, color: "var(--muted)" }}>server name</span>
              <input
                onChange={(e) => setServerName(e.target.value)}
                style={{ minWidth: 200 }}
                value={serverName}
              />
            </label>
            <button className="primary-button" disabled={enrolling} type="submit">
              {enrolling ? "Enrolling..." : "Enroll"}
            </button>
            <button
              className="secondary-button"
              onClick={(e) => {
                e.preventDefault();
                regenerateInstallationId();
              }}
              type="button"
            >
              新 installation_id
            </button>
          </form>
          {enrollError ? <p className="flash">{enrollError}</p> : null}
          {enrolled ? (
            <div className="table-card">
              <div className="table-title">
                <strong>已注册</strong>
                <span>幂等键 = (account_id, installation_id, name)</span>
              </div>
              <table>
                <tbody>
                  <tr>
                    <th style={{ width: 180 }}>server_id</th>
                    <td>
                      <code>{enrolled.serverId}</code>
                    </td>
                  </tr>
                  <tr>
                    <th>environment_id</th>
                    <td>
                      <code>{enrolled.environmentId}</code>
                    </td>
                  </tr>
                </tbody>
              </table>
            </div>
          ) : null}
          <p style={{ marginTop: 12, fontSize: 13, color: "var(--muted)" }}>
            后端: <code>{bootstrap.backendBaseUrl}</code> · 协议: <code>v{bootstrap.protocolVersion}</code> · strict-account-header:{" "}
            <code>{bootstrap.strictAccountHeader ? "true" : "false"}</code>
          </p>
        </section>

        <section className="content-card">
          <h2>2. Phone WSS</h2>
          <p className="lead" style={{ marginBottom: 12 }}>
            模拟 ChatGPT App 端连接 <code>/wham/remote/control/client</code>。状态:{" "}
            <strong>{statusLabel(phoneStatus)}</strong>
            {phoneCloseCode !== null ? <> · 上次关闭 code <code>{phoneCloseCode}</code></> : null}
          </p>
          <div className="inline-form" style={{ flexWrap: "wrap" }}>
            <label>
              <span style={{ display: "block", fontSize: 12, color: "var(--muted)" }}>client_id</span>
              <input onChange={(e) => setClientId(e.target.value)} style={{ minWidth: 260 }} value={clientId} />
            </label>
            <button className="secondary-button" onClick={regenerateClientId} type="button">
              新 client_id
            </button>
            <label>
              <span style={{ display: "block", fontSize: 12, color: "var(--muted)" }}>stream_id</span>
              <input onChange={(e) => setStreamId(e.target.value)} style={{ minWidth: 260 }} value={streamId} />
            </label>
            <button className="secondary-button" onClick={regenerateStreamId} type="button">
              新 stream_id
            </button>
            <button
              className="primary-button"
              disabled={!phoneCanConnect}
              onClick={connectPhone}
              type="button"
            >
              连接 Phone
            </button>
            <button
              className="secondary-button"
              disabled={!phoneCanDisconnect}
              onClick={disconnectPhone}
              type="button"
            >
              断开 Phone
            </button>
          </div>
          <p style={{ marginTop: 8, fontSize: 12, color: "var(--muted)" }}>
            订阅游标 (subscribe_cursor):{" "}
            <code>{phoneSubscribeCursorRef.current ?? "—"}</code> · 已观察 seq_id 最大值:{" "}
            <code>{phoneHighestSeqRef.current ?? "—"}</code>
          </p>
          <div style={{ marginTop: 12 }}>
            <span style={{ display: "block", fontSize: 12, color: "var(--muted)", marginBottom: 4 }}>
              要从 Phone 发出的 envelope (JSON)
            </span>
            <textarea
              onChange={(e) => setPhoneDraft(e.target.value)}
              rows={8}
              style={{
                width: "100%",
                fontFamily: "Berkeley Mono, SFMono-Regular, Consolas, monospace",
                background: "rgba(27, 28, 30, 0.04)",
                border: "1px solid var(--line)",
                borderRadius: 8,
                padding: 10,
              }}
              value={phoneDraft}
            />
          </div>
          <div className="inline-form" style={{ marginTop: 8 }}>
            <button className="primary-button" onClick={sendFromPhone} type="button">
              Phone → 发送
            </button>
            <button className="secondary-button" onClick={sendAckFromPhone} type="button">
              发送 Ack(最高 seq_id)
            </button>
            <button
              className="secondary-button"
              onClick={() => sendClientClosedFromPhone("stream")}
              type="button"
            >
              ClientClosed (this stream)
            </button>
            <button
              className="secondary-button"
              onClick={() => sendClientClosedFromPhone("client")}
              type="button"
            >
              ClientClosed (all streams)
            </button>
          </div>
        </section>

        <section className="content-card">
          <h2>3. Codex WSS (可选)</h2>
          <p className="lead" style={{ marginBottom: 12 }}>
            把浏览器伪装成 Codex 本地端连接 <code>/wham/remote/control/server</code>，用来给同环境下
            的 Phone 发回包。状态: <strong>{statusLabel(codexStatus)}</strong>
            {codexCloseCode !== null ? <> · 上次关闭 code <code>{codexCloseCode}</code></> : null}
          </p>
          <div className="inline-form" style={{ flexWrap: "wrap" }}>
            <button
              className="primary-button"
              disabled={!codexCanConnect}
              onClick={connectCodex}
              type="button"
            >
              连接 Codex
            </button>
            <button
              className="secondary-button"
              disabled={!codexCanDisconnect}
              onClick={disconnectCodex}
              type="button"
            >
              断开 Codex
            </button>
          </div>
          <p style={{ marginTop: 8, fontSize: 12, color: "var(--muted)" }}>
            订阅游标 (x-codex-subscribe-cursor):{" "}
            <code>{codexSubscribeCursorRef.current ?? "—"}</code>
          </p>
          <div style={{ marginTop: 12 }}>
            <span style={{ display: "block", fontSize: 12, color: "var(--muted)", marginBottom: 4 }}>
              要从 Codex 发出的 envelope (JSON)
            </span>
            <textarea
              onChange={(e) => setCodexDraft(e.target.value)}
              rows={8}
              style={{
                width: "100%",
                fontFamily: "Berkeley Mono, SFMono-Regular, Consolas, monospace",
                background: "rgba(27, 28, 30, 0.04)",
                border: "1px solid var(--line)",
                borderRadius: 8,
                padding: 10,
              }}
              value={codexDraft}
            />
          </div>
          <div className="inline-form" style={{ marginTop: 8 }}>
            <button className="primary-button" onClick={sendFromCodex} type="button">
              Codex → 发送
            </button>
          </div>
        </section>

        <section className="content-card">
          <h2>4. Log</h2>
          <div className="inline-form" style={{ marginBottom: 8 }}>
            <button className="secondary-button" onClick={clearLog} type="button">
              清空
            </button>
            <span style={{ fontSize: 12, color: "var(--muted)" }}>
              最近 500 条 · phone-out / codex-out 是浏览器发出, *-in 是收到
            </span>
          </div>
          <div
            style={{
              maxHeight: 360,
              overflowY: "auto",
              border: "1px solid var(--line)",
              borderRadius: 8,
              background: "rgba(255,255,255,0.6)",
            }}
          >
            <table>
              <thead>
                <tr>
                  <th style={{ width: 120 }}>时间</th>
                  <th style={{ width: 110 }}>方向</th>
                  <th>内容</th>
                </tr>
              </thead>
              <tbody>
                {log.length === 0 ? (
                  <tr>
                    <td colSpan={3}>暂无消息。</td>
                  </tr>
                ) : (
                  log
                    .slice()
                    .reverse()
                    .map((entry) => (
                      <tr key={entry.id}>
                        <td>
                          <code>{entry.ts}</code>
                        </td>
                        <td>
                          <span style={{ color: directionColor(entry.direction) }}>{entry.direction}</span>
                        </td>
                        <td>
                          <pre style={{ margin: 0, whiteSpace: "pre-wrap", wordBreak: "break-all" }}>
                            {entry.text}
                          </pre>
                        </td>
                      </tr>
                    ))
                )}
              </tbody>
            </table>
          </div>
        </section>
      </section>
    </main>
  );
}

function directionColor(direction: LogDirection) {
  switch (direction) {
    case "phone-out":
      return "#1d4ed8";
    case "phone-in":
      return "#0f766e";
    case "codex-out":
      return "#7c3aed";
    case "codex-in":
      return "#b45309";
    case "error":
      return "#9f1239";
    default:
      return "var(--muted)";
  }
}

function App() {
  const bootstrap = readBootstrap();

  if (bootstrap.page === "browserLogin") {
    return <BrowserLoginPage bootstrap={bootstrap} />;
  }
  if (bootstrap.page === "accountConfirm") {
    return <AccountConfirmPage bootstrap={bootstrap} />;
  }
  if (bootstrap.page === "deviceAuth") {
    return <DeviceAuthPage bootstrap={bootstrap} />;
  }
  if (bootstrap.page === "taskView") {
    return <TaskPage bootstrap={bootstrap} />;
  }
  if (bootstrap.page === "remoteControl") {
    return <RemoteControlPage bootstrap={bootstrap} />;
  }
  return <CallbackPage />;
}

const root = document.getElementById("root");
if (!root) {
  throw new Error("missing root element");
}

createRoot(root).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
