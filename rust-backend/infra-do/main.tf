data "digitalocean_vpc" "main" {
  name = var.vpc_name
}

resource "digitalocean_ssh_key" "deploy" {
  name       = "options-deploy"
  public_key = var.deploy_ssh_pubkey
}

resource "digitalocean_ssh_key" "operator" {
  count      = var.operator_ssh_pubkey != "" ? 1 : 0
  name       = "options-operator"
  public_key = var.operator_ssh_pubkey
}

locals {
  ssh_key_ids = concat(
    [digitalocean_ssh_key.deploy.fingerprint],
    var.operator_ssh_pubkey != "" ? [digitalocean_ssh_key.operator[0].fingerprint] : []
  )
}

# ---- Staging + monitoring host (edge nginx, compose stacks, Tailscale) ------
resource "digitalocean_droplet" "staging" {
  name      = "options-host-do"
  region    = var.region
  size      = var.staging_droplet_size
  image     = var.droplet_image
  vpc_uuid  = data.digitalocean_vpc.main.id
  ssh_keys  = local.ssh_key_ids
  user_data = format("#!/bin/bash\nexport ROLE=staging\n%s", file("${path.module}/../deployment/do/host-bootstrap.sh"))
  tags      = ["options", "staging"]
}

resource "digitalocean_reserved_ip" "ingress" {
  region = var.region
}

resource "digitalocean_reserved_ip_assignment" "ingress" {
  ip_address = digitalocean_reserved_ip.ingress.ip_address
  droplet_id = digitalocean_droplet.staging.id
}

# ---- Data-room host ---------------------------------------------------------
resource "digitalocean_droplet" "data_room" {
  name      = "options-data-room-host-do"
  region    = var.region
  size      = var.data_room_droplet_size
  image     = var.droplet_image
  vpc_uuid  = data.digitalocean_vpc.main.id
  ssh_keys  = local.ssh_key_ids
  user_data = format("#!/bin/bash\nexport ROLE=data-room\n%s", file("${path.module}/../deployment/do/host-bootstrap.sh"))
  tags      = ["options", "data-room"]
}

resource "digitalocean_volume" "data_room_spool" {
  name                    = "options-data-room-spool"
  region                  = var.region
  size                    = var.data_room_volume_gb
  initial_filesystem_type = "ext4"
}

resource "digitalocean_volume_attachment" "data_room_spool" {
  droplet_id = digitalocean_droplet.data_room.id
  volume_id  = digitalocean_volume.data_room_spool.id
}

# ---- Firewalls --------------------------------------------------------------
# SSH is key-only (PasswordAuthentication is off on the DO Ubuntu image).
resource "digitalocean_firewall" "staging" {
  name        = "options-staging"
  droplet_ids = [digitalocean_droplet.staging.id]

  inbound_rule {
    protocol         = "tcp"
    port_range       = "22"
    source_addresses = ["0.0.0.0/0", "::/0"]
  }
  inbound_rule {
    protocol         = "tcp"
    port_range       = "80"
    source_addresses = ["0.0.0.0/0", "::/0"]
  }
  inbound_rule {
    protocol         = "tcp"
    port_range       = "443"
    source_addresses = ["0.0.0.0/0", "::/0"]
  }
  # Loki push + Prometheus remote-write + OTLP from VPC peers (future prod host)
  inbound_rule {
    protocol         = "tcp"
    port_range       = "3100"
    source_addresses = [data.digitalocean_vpc.main.ip_range]
  }
  inbound_rule {
    protocol         = "tcp"
    port_range       = "4318"
    source_addresses = [data.digitalocean_vpc.main.ip_range]
  }
  inbound_rule {
    protocol         = "tcp"
    port_range       = "9090"
    source_addresses = [data.digitalocean_vpc.main.ip_range]
  }
  # Tailscale (subnet router for laptop -> managed PG access)
  inbound_rule {
    protocol         = "udp"
    port_range       = "41641"
    source_addresses = ["0.0.0.0/0", "::/0"]
  }

  outbound_rule {
    protocol              = "tcp"
    port_range            = "1-65535"
    destination_addresses = ["0.0.0.0/0", "::/0"]
  }
  outbound_rule {
    protocol              = "udp"
    port_range            = "1-65535"
    destination_addresses = ["0.0.0.0/0", "::/0"]
  }
  outbound_rule {
    protocol              = "icmp"
    destination_addresses = ["0.0.0.0/0", "::/0"]
  }
}

resource "digitalocean_firewall" "data_room" {
  name        = "options-data-room"
  droplet_ids = [digitalocean_droplet.data_room.id]

  inbound_rule {
    protocol         = "tcp"
    port_range       = "22"
    source_addresses = ["0.0.0.0/0", "::/0"]
  }
  # node-exporter / collector metrics scraped by central Prometheus over the VPC
  inbound_rule {
    protocol         = "tcp"
    port_range       = "9100"
    source_addresses = [data.digitalocean_vpc.main.ip_range]
  }

  outbound_rule {
    protocol              = "tcp"
    port_range            = "1-65535"
    destination_addresses = ["0.0.0.0/0", "::/0"]
  }
  outbound_rule {
    protocol              = "udp"
    port_range            = "1-65535"
    destination_addresses = ["0.0.0.0/0", "::/0"]
  }
  outbound_rule {
    protocol              = "icmp"
    destination_addresses = ["0.0.0.0/0", "::/0"]
  }
}

# ---- Project membership -----------------------------------------------------
data "digitalocean_project" "options" {
  name = "options"
}

resource "digitalocean_project_resources" "options" {
  project = data.digitalocean_project.options.id
  resources = [
    digitalocean_droplet.staging.urn,
    digitalocean_droplet.data_room.urn,
    digitalocean_volume.data_room_spool.urn,
    digitalocean_reserved_ip.ingress.urn,
  ]
}

output "staging_public_ip" {
  value = digitalocean_droplet.staging.ipv4_address
}
output "staging_private_ip" {
  value = digitalocean_droplet.staging.ipv4_address_private
}
output "data_room_public_ip" {
  value = digitalocean_droplet.data_room.ipv4_address
}
output "data_room_private_ip" {
  value = digitalocean_droplet.data_room.ipv4_address_private
}
output "reserved_ip" {
  value = digitalocean_reserved_ip.ingress.ip_address
}

# ---- NFT marketplace host (aptos-nft-marketplace) ---------------------------
# Cost-sized sibling of staging: same image/region/VPC/keys, smaller plan,
# no volume, no backups. All service state is rebuildable (Postgres reseeds
# from chain; images in DOCR; secrets in SOPS + GH secrets).

resource "digitalocean_droplet" "nft" {
  name      = "options-nft-host-do"
  size      = var.nft_droplet_size
  image     = var.droplet_image
  region    = var.region
  vpc_uuid  = data.digitalocean_vpc.main.id
  ssh_keys  = local.ssh_key_ids
  backups   = false
  user_data = format("#!/bin/bash\nexport ROLE=nft\n%s", file("${path.module}/../deployment/do/host-bootstrap.sh"))
  tags      = ["options", "nft"]
}

resource "digitalocean_reserved_ip" "nft" {
  region     = var.region
  droplet_id = digitalocean_droplet.nft.id
}

resource "digitalocean_record" "nft" {
  domain = "sui-options.com"
  type   = "A"
  name   = "nft"
  value  = digitalocean_reserved_ip.nft.ip_address
}

resource "digitalocean_firewall" "nft" {
  name        = "options-nft"
  droplet_ids = [digitalocean_droplet.nft.id]

  inbound_rule {
    protocol         = "tcp"
    port_range       = "22"
    source_addresses = ["0.0.0.0/0", "::/0"]
  }
  inbound_rule {
    protocol         = "tcp"
    port_range       = "80"
    source_addresses = ["0.0.0.0/0", "::/0"]
  }
  inbound_rule {
    protocol         = "tcp"
    port_range       = "443"
    source_addresses = ["0.0.0.0/0", "::/0"]
  }

  outbound_rule {
    protocol              = "tcp"
    port_range            = "1-65535"
    destination_addresses = ["0.0.0.0/0", "::/0"]
  }
  outbound_rule {
    protocol              = "udp"
    port_range            = "1-65535"
    destination_addresses = ["0.0.0.0/0", "::/0"]
  }
}

resource "digitalocean_project" "nft" {
  name = "options-nft"
  resources = [
    digitalocean_droplet.nft.urn,
    digitalocean_reserved_ip.nft.urn,
  ]
}

output "nft_public_ip" {
  value = digitalocean_reserved_ip.nft.ip_address
}

output "nft_domain" {
  value = var.nft_domain
}
