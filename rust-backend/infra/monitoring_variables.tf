# ---- Monitoring (Grafana + Loki) ---------------------------------------------

variable "grafana_admin_password" {
  description = "Initial Grafana admin password. Change via UI after first login."
  type        = string
  sensitive   = true
  default     = ""
}
