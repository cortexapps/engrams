{{/*
Helpers for the host-fleet chart. Two DaemonSets — `node-prep` and
`host-agent` — share a release identity and differ on
`app.kubernetes.io/component`. Names follow `{release}-host-agent` /
`{release}-node-prep`.
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

{{- define "hostfleet.nodePrep.fullname" -}}
{{- printf "%s-node-prep" (include "hostfleet.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "hostfleet.nodePrep.labels" -}}
{{ include "hostfleet.labels" . }}
app.kubernetes.io/component: node-prep
{{- end -}}

{{- define "hostfleet.nodePrep.selectorLabels" -}}
app.kubernetes.io/name: {{ .Chart.Name }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: node-prep
{{- end -}}

{{- define "hostfleet.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "hostfleet.hostAgent.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}
