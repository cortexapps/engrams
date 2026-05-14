{{/*
Helpers. One chart, two components — coordinator + web — share a
release-level identity (`app.kubernetes.io/instance`) and differ on
`app.kubernetes.io/component`. Resource names follow
`{release}-coordinator` / `{release}-web` so an operator scanning
`kubectl get all -n engram` sees the topology at a glance.
*/}}

{{- define "engram.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "engram.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "engram.labels" -}}
helm.sh/chart: {{ include "engram.chart" . }}
app.kubernetes.io/name: {{ .Chart.Name }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/part-of: engram
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{/* ─── coordinator ───────────────────────────────────────────── */}}

{{- define "engram.coordinator.fullname" -}}
{{- printf "%s-coordinator" (include "engram.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "engram.coordinator.labels" -}}
{{ include "engram.labels" . }}
app.kubernetes.io/component: coordinator
{{- end -}}

{{- define "engram.coordinator.selectorLabels" -}}
app.kubernetes.io/name: {{ .Chart.Name }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: coordinator
{{- end -}}

{{- define "engram.coordinator.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "engram.coordinator.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{/* ─── web ───────────────────────────────────────────────────── */}}

{{- define "engram.web.fullname" -}}
{{- printf "%s-web" (include "engram.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "engram.web.labels" -}}
{{ include "engram.labels" . }}
app.kubernetes.io/component: web
{{- end -}}

{{- define "engram.web.selectorLabels" -}}
app.kubernetes.io/name: {{ .Chart.Name }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: web
{{- end -}}
