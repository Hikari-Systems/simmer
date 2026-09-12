#!/usr/bin/env python3
"""Probe Postal's SMTP server and verify delivery into Mailpit. Stdlib only.

Env: POSTAL_HOST (smtp), POSTAL_PORT (25), MAILPIT_API (http://mailpit:8025),
     SMTP_PASSWORD, SENDER_DOMAIN.
Writes probe-results.json next to this file.
"""
import base64, hmac, json, os, smtplib, socket, ssl, sys, time, urllib.request, uuid
from email.message import EmailMessage

HOST = os.environ.get("POSTAL_HOST", "smtp")
PORT = int(os.environ.get("POSTAL_PORT", "25"))
API = os.environ.get("MAILPIT_API", "http://mailpit:8025")
PW = os.environ.get("SMTP_PASSWORD", "spike-smtp-password-0001")
DOM = os.environ.get("SENDER_DOMAIN", "sender.test")
results = {}


def conn():
    s = smtplib.SMTP(HOST, PORT, timeout=30)
    return s


def tls_ctx():
    c = ssl.create_default_context()
    c.check_hostname = False
    c.verify_mode = ssl.CERT_NONE
    return c


def raw_session(lines, read_banner=True, pipeline=False):
    """Send raw lines; return transcript. pipeline=True writes all at once."""
    s = socket.create_connection((HOST, PORT), timeout=15)
    f = s.makefile("rb")
    out = []

    def read_reply():
        rep = []
        while True:
            l = f.readline().decode("utf-8", "replace").rstrip("\r\n")
            rep.append(l)
            if len(l) < 4 or l[3] != "-":
                return rep

    if read_banner:
        out.append(("S", read_reply()))
    if pipeline:
        s.sendall(b"".join(l.encode() + b"\r\n" for l in lines))
        for l in lines:
            out.append(("C", l))
            out.append(("S", read_reply()))
    else:
        for l in lines:
            s.sendall(l.encode() + b"\r\n")
            out.append(("C", l))
            out.append(("S", read_reply()))
    s.close()
    return out


def mailpit(path):
    with urllib.request.urlopen(API + path, timeout=10) as r:
        return json.loads(r.read())


def mk(frm, subject, body="hello from the postal spike\n", to="rcpt@example.net"):
    m = EmailMessage()
    m["From"] = frm
    m["To"] = to
    m["Subject"] = subject
    m["Message-ID"] = f"<{uuid.uuid4()}@client.spike>"
    m.set_content(body)
    return m


# 1. Banner + EHLO verbatim, before and after STARTTLS
tr = raw_session(["EHLO probe.client", "QUIT"])
results["banner"] = tr[0][1]
results["ehlo_plain"] = tr[2][1]
s = conn()
s.ehlo("probe.client")
code, msg = s.starttls(context=tls_ctx())
results["starttls_reply"] = [code, msg.decode()]
code, msg = s.ehlo("probe.client")
results["ehlo_after_starttls"] = [code, msg.decode()]
results["tls_version"] = s.sock.version()
results["tls_cipher"] = s.sock.cipher()[0]
s.quit()

# 2. Wrong password (PLAIN, LOGIN), CRAM-MD5 wrong
for mech in ("PLAIN", "LOGIN"):
    s = conn(); s.ehlo("probe.client")
    try:
        s.user, s.password = "anything", "wrong-password"
        code, msg = s.auth(mech, getattr(s, "auth_" + mech.lower()), initial_response_ok=True)
        results[f"auth_{mech}_wrong"] = [code, msg.decode()]
    except smtplib.SMTPAuthenticationError as e:
        results[f"auth_{mech}_wrong"] = [e.smtp_code, e.smtp_error.decode()]
    s.close()

# 3. Correct auth, each mechanism
for mech, user in (("PLAIN", "ignored-user"), ("LOGIN", "ignored-user"), ("CRAM-MD5", "spike/relay")):
    s = conn(); s.ehlo("probe.client")
    s.user, s.password = user, PW
    fn = {"PLAIN": s.auth_plain, "LOGIN": s.auth_login, "CRAM-MD5": s.auth_cram_md5}[mech]
    try:
        code, msg = s.auth(mech, fn, initial_response_ok=True)
        results[f"auth_{mech}_ok"] = [code, msg.decode()]
    except smtplib.SMTPException as e:
        results[f"auth_{mech}_ok"] = ["ERR", repr(e)]
    s.close()

# 4. Unauthenticated relay attempt
tr = raw_session(["EHLO probe.client", "MAIL FROM:<a@sender.test>", "RCPT TO:<rcpt@example.net>", "QUIT"])
results["unauth_rcpt"] = tr[6][1]

# 5. Unknown sender domain (From header on a domain Postal has not verified)
s = conn(); s.ehlo("probe.client"); s.login("x", PW)
try:
    s.send_message(mk("someone@unknown-domain.test", "unknown domain"), from_addr="someone@unknown-domain.test",
                   to_addrs=["rcpt@example.net"])
    results["unknown_from_domain"] = ["ACCEPTED?!"]
except smtplib.SMTPDataError as e:
    results["unknown_from_domain"] = [e.smtp_code, e.smtp_error.decode()]
s.close()

# 5b. Envelope sender on unknown domain but From header verified
s = conn(); s.ehlo("probe.client"); s.login("x", PW)
subj_env = f"envelope-unknown-{uuid.uuid4().hex[:8]}"
try:
    s.send_message(mk(f"app@{DOM}", subj_env), from_addr="bounce@unknown-domain.test", to_addrs=["rcpt2@example.net"])
    results["unknown_envelope_verified_from"] = [250, "accepted"]
except smtplib.SMTPException as e:
    results["unknown_envelope_verified_from"] = ["ERR", repr(e)]
s.close()

# 6. Happy path: accepted message (STARTTLS + AUTH PLAIN), capture final reply via low-level calls
subject = f"spike-{uuid.uuid4().hex[:8]}"
orig = mk(f"App Sender <app@{DOM}>", subject)
orig_msgid = orig["Message-ID"]
s = conn(); s.ehlo("probe.client"); s.starttls(context=tls_ctx()); s.ehlo("probe.client"); s.login("x", PW)
results["mail_from_reply"] = [*map(lambda v: v.decode() if isinstance(v, bytes) else v, s.mail(f"app@{DOM}"))]
results["rcpt_reply"] = [*map(lambda v: v.decode() if isinstance(v, bytes) else v, s.rcpt("rcpt@example.net"))]
t_sent = time.time()
code, msg = s.data(orig.as_bytes())
results["data_reply"] = [code, msg.decode()]
s.quit()

# 7. Too large (15 MB > 14 MB default)
s = conn(); s.ehlo("probe.client"); s.login("x", PW)
big = mk(f"app@{DOM}", "too big", body=("x" * 998 + "\n") * (15 * 1024))
try:
    s.send_message(big, to_addrs=["rcpt@example.net"])
    results["too_large"] = ["ACCEPTED?!"]
except smtplib.SMTPException as e:
    results["too_large"] = [getattr(e, "smtp_code", None), getattr(e, "smtp_error", b"").decode() or repr(e)]
s.close()

# 8. Pipelining (not advertised): write MAIL/RCPT/DATA in one go after AUTH
auth = base64.b64encode(f"\0x\0{PW}".encode()).decode()
tr = raw_session(["EHLO probe.client", f"AUTH PLAIN {auth}"])
s = socket.create_connection((HOST, PORT), timeout=15); f = s.makefile("rb")
f.readline()
s.sendall(b"EHLO probe.client\r\n"); [f.readline() for _ in range(3)]
s.sendall(f"AUTH PLAIN {auth}\r\n".encode()); f.readline()
psubj = f"pipelined-{uuid.uuid4().hex[:8]}"
s.sendall((f"MAIL FROM:<app@{DOM}>\r\nRCPT TO:<rcpt3@example.net>\r\nDATA\r\n").encode())
pr = [f.readline().decode().strip() for _ in range(3)]
s.sendall((f"From: app@{DOM}\r\nTo: rcpt3@example.net\r\nSubject: {psubj}\r\n\r\nPipelined body \xe9\xe8 UTF-8 ü\r\n.\r\n").encode("utf-8"))
pr.append(f.readline().decode().strip())
s.sendall(b"QUIT\r\n"); pr.append(f.readline().decode().strip()); s.close()
results["pipelined_replies"] = pr

# 9. SMTPUTF8 not advertised: smtplib refuses SMTPUTF8 option; try raw UTF-8 local part
tr = raw_session(["EHLO probe.client", f"AUTH PLAIN {auth}", f"MAIL FROM:<app@{DOM}> SMTPUTF8",
                  "RCPT TO:<jürgen@example.net>", "RSET", f"MAIL FROM:<app@{DOM}> BODY=8BITMIME", "QUIT"])
results["smtputf8_8bitmime_transcript"] = tr[3:]

# 10. Wait for Mailpit delivery
found = None
deadline = time.time() + 60
while time.time() < deadline:
    msgs = mailpit("/api/v1/messages?limit=50")["messages"]
    hit = [m for m in msgs if m["Subject"] == subject]
    if hit:
        found = hit[0]; break
    time.sleep(0.5)
results["delivered"] = bool(found)
if found:
    results["delivery_latency_s"] = round(time.time() - t_sent, 2)
    mid = found["ID"]
    results["mailpit_summary"] = mailpit(f"/api/v1/message/{mid}")
    with urllib.request.urlopen(f"{API}/api/v1/message/{mid}/raw", timeout=10) as r:
        results["raw_as_delivered"] = r.read().decode("utf-8", "replace")
    results["original_message_id"] = orig_msgid
    results["mailpit_headers"] = mailpit(f"/api/v1/message/{mid}/headers")
time.sleep(3)
allm = mailpit("/api/v1/messages?limit=50")["messages"]
results["mailpit_all"] = [{"Subject": m["Subject"], "To": m["To"], "From": m["From"]} for m in allm]
envm = [m for m in allm if m["Subject"] == subj_env]
if envm:
    results["envelope_unknown_raw_head"] = urllib.request.urlopen(f"{API}/api/v1/message/{envm[0]['ID']}/raw").read().decode("utf-8","replace")[:1500]
pm = [m for m in allm if m["Subject"] == psubj]
if pm:
    results["pipelined_raw"] = urllib.request.urlopen(f"{API}/api/v1/message/{pm[0]['ID']}/raw").read().decode("utf-8","replace")

out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "probe-results.json")
json.dump(results, open(out, "w"), indent=2, default=str)
print(json.dumps({k: v for k, v in results.items() if k not in ("mailpit_summary", "raw_as_delivered", "mailpit_headers", "pipelined_raw", "envelope_unknown_raw_head")}, indent=2, default=str))
sys.exit(0 if found else 1)
