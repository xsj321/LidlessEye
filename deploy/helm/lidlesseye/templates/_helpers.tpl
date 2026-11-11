{{/* Common name/label helpers. */}}
{{- define "lidlesseye.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "lidlesseye.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := default .Chart.Name .Values.nameOverride -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{- define "lidlesseye.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "lidlesseye.labels" -}}
helm.sh/chart: {{ include "lidlesseye.chart" . }}
app.kubernetes.io/name: {{ include "lidlesseye.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/part-of: lidlesseye
{{- end -}}

{{- define "lidlesseye.selectorLabels" -}}
app.kubernetes.io/name: {{ include "lidlesseye.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "lidlesseye.server.labels" -}}
{{ include "lidlesseye.labels" . }}
app.kubernetes.io/component: server
{{- end -}}

{{- define "lidlesseye.server.selectorLabels" -}}
{{ include "lidlesseye.selectorLabels" . }}
app.kubernetes.io/component: server
{{- end -}}

{{- define "lidlesseye.agent.labels" -}}
{{ include "lidlesseye.labels" . }}
app.kubernetes.io/component: agent
{{- end -}}

{{- define "lidlesseye.agent.selectorLabels" -}}
{{ include "lidlesseye.selectorLabels" . }}
app.kubernetes.io/component: agent
{{- end -}}

{{- define "lidlesseye.image" -}}
{{- printf "%s:%s" .Values.image.repository (default .Chart.AppVersion .Values.image.tag) -}}
{{- end -}}

{{- define "lidlesseye.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "lidlesseye.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{/* Name of the secret holding the API token, or "" when auth is off. */}}
{{- define "lidlesseye.authSecretName" -}}
{{- if .Values.server.auth.existingSecret -}}
{{- .Values.server.auth.existingSecret -}}
{{- else if .Values.server.auth.enabled -}}
{{- printf "%s-auth" (include "lidlesseye.fullname" .) -}}
{{- end -}}
{{- end -}}

{{/* In-cluster URL of the server, used by the agent. */}}
{{- define "lidlesseye.serverUrl" -}}
{{- printf "http://%s:%v" (include "lidlesseye.fullname" .) .Values.server.service.port -}}
{{- end -}}
