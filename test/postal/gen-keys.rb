# Generates /config/signing.key (Postal's required RSA signing key) and a self-signed
# SMTP TLS cert/key for STARTTLS, unless files already exist (so supplied PEMs win).
require "openssl"

dir = ENV.fetch("SPIKE_CONFIG_DIR", "/config")
host = ENV.fetch("POSTAL_SMTP_HOSTNAME", "postal.test")

signing = File.join(dir, "signing.key")
File.write(signing, OpenSSL::PKey::RSA.new(2048).to_pem) unless File.exist?(signing)

cert_path = File.join(dir, "smtp.cert")
key_path  = File.join(dir, "smtp.key")
unless File.exist?(cert_path) && File.exist?(key_path)
  key = OpenSSL::PKey::RSA.new(2048)
  name = OpenSSL::X509::Name.parse("/CN=#{host}")
  cert = OpenSSL::X509::Certificate.new
  cert.version = 2
  cert.serial = 1
  cert.subject = name
  cert.issuer = name
  cert.public_key = key.public_key
  cert.not_before = Time.now - 60
  cert.not_after = Time.now + (365 * 86_400)
  ef = OpenSSL::X509::ExtensionFactory.new(cert, cert)
  cert.add_extension(ef.create_extension("subjectAltName", "DNS:#{host},DNS:smtp", false))
  cert.sign(key, OpenSSL::Digest.new("SHA256"))
  File.write(key_path, key.to_pem)
  File.write(cert_path, cert.to_pem)
end

Dir[File.join(dir, "*")].each { |f| File.chmod(0o644, f) }
puts "keys ready in #{dir}: #{Dir.children(dir).sort.join(', ')}"
