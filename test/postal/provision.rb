# Postal 3.3.7 headless provisioning -- run with `bundle exec rails runner provision.rb`.
# Idempotent: safe to run on every stack start.
#
# Creates: admin user (Organization.owner is a required belongs_to), organization,
# a Live-mode mail server (Development mode HOLDS every outgoing message), a verified
# sending domain, and an SMTP credential whose key (= the SMTP AUTH password) is forced to a
# known value. Postal ignores the SMTP AUTH username for PLAIN/LOGIN; only the key matters.

cred_key      = ENV.fetch("SPIKE_SMTP_PASSWORD", "spike-smtp-password-0001")
sender_domain = ENV.fetch("SPIKE_SENDER_DOMAIN", "sender.test")
use_for_any   = ENV.fetch("SPIKE_DOMAIN_USE_FOR_ANY", "false") == "true"

user = User.find_by(email_address: "admin@postal.test") || User.create!(
  first_name: "Spike", last_name: "Admin", email_address: "admin@postal.test",
  password: "spike-admin-password", admin: true, email_verified_at: Time.now
)

org = Organization.find_by(permalink: "spike") ||
      Organization.create!(name: "Spike", permalink: "spike", owner: user)

# after_create provisions the per-server message DB (CREATE DATABASE `postal-server-<id>`),
# so the message_db user needs CREATE privileges.
server = org.servers.find_by(permalink: "relay") ||
         org.servers.create!(name: "Relay", permalink: "relay", mode: "Live")
server.update!(mode: "Live") unless server.mode == "Live"

# The SMTP server checks the *From header* (not MAIL FROM) against verified domains
# owned by the server or its organization; failure => "530 From/Sender name is not valid".
domain = server.domains.find_by(name: sender_domain) ||
         server.domains.create!(name: sender_domain, verification_method: "DNS")
domain.update_columns(verified_at: Time.now) unless domain.verified?
domain.update_columns(use_for_any: use_for_any) unless domain.use_for_any == use_for_any

# Credential#generate_key overwrites any key given at create time with a random
# 24-char string, so set the known value afterwards with update_column (skips the
# "key cannot be changed" validation).
cred = server.credentials.find_by(name: "spike") ||
       server.credentials.create!(type: "SMTP", name: "spike")
cred.update_column(:key, cred_key) unless cred.key == cred_key

puts({ organization: org.permalink, server: server.full_permalink, server_mode: server.mode,
       server_token: server.token, domain: domain.name, domain_verified: domain.reload.verified?,
       use_for_any: domain.use_for_any, credential_type: cred.type,
       credential_key: cred.reload.key }.to_json)
