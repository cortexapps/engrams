{{/*
Helpers for the host-fleet chart. The host-agent DaemonSet owns node
preparation as its first init container, so host setup and asset staging
have one ordered lifecycle.
*/}}

{{- define "hostfleet.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "hostfleet.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "hostfleet.labels" -}}
helm.sh/chart: {{ include "hostfleet.chart" . }}
app.kubernetes.io/name: {{ .Chart.Name }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/part-of: engram
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{- define "hostfleet.hostAgent.fullname" -}}
{{- printf "%s-host-agent" (include "hostfleet.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "hostfleet.hostAgent.labels" -}}
{{ include "hostfleet.labels" . }}
app.kubernetes.io/component: host-agent
{{- end -}}

{{- define "hostfleet.hostAgent.selectorLabels" -}}
app.kubernetes.io/name: {{ .Chart.Name }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: host-agent
{{- end -}}

{{- define "hostfleet.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "hostfleet.hostAgent.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{/* ADR 0044 K3: the rollout operator (a Deployment, not a DaemonSet). */}}
{{- define "hostfleet.operator.fullname" -}}
{{- printf "%s-operator" (include "hostfleet.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "hostfleet.operator.labels" -}}
{{ include "hostfleet.labels" . }}
app.kubernetes.io/component: operator
{{- end -}}

{{- define "hostfleet.operator.selectorLabels" -}}
app.kubernetes.io/name: {{ .Chart.Name }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: operator
{{- end -}}
