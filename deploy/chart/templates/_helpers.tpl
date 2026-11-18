{{- define "utopia.image" -}}
{{- if .Values.image.digest -}}
{{ .Values.image.repository }}@{{ .Values.image.digest }}
{{- else -}}
{{ .Values.image.repository }}:{{ .Values.image.tag }}
{{- end -}}
{{- end -}}

{{- define "utopia.url" -}}
https://{{ .Values.hostname }}
{{- end -}}

{{/* nodeSelector and tolerations: the chart's own values, else global.* */}}
{{- define "utopia.scheduling" -}}
{{- $global := .Values.global | default dict -}}
{{- $rules := dict -}}
{{- with .Values.nodeSelector | default $global.nodeSelector }}{{- $_ := set $rules "nodeSelector" . }}{{- end -}}
{{- with .Values.tolerations | default $global.tolerations }}{{- $_ := set $rules "tolerations" . }}{{- end -}}
{{- if $rules }}{{ toYaml $rules }}{{- end -}}
{{- end -}}
