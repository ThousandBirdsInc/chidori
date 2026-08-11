'use client';

/**
 * The live half of a form card: react-jsonschema-form driving the journaled
 * JSON Schema. This module pulls in @rjsf/core + ajv, so cards.tsx loads it
 * dynamically — visitors only download it when a chat actually renders a
 * form. Styling rides on the `.playground-form` rules in global.css, which
 * restyle RJSF's default (bootstrap-flavored) markup with the site's tokens.
 */
import Form from '@rjsf/core';
import type { RJSFSchema, UiSchema } from '@rjsf/utils';
import validator from '@rjsf/validator-ajv8';
import type { FormData, FormSpec } from './form-schema';

export function SchemaForm({
  spec,
  disabled,
  onSubmit,
}: {
  spec: FormSpec;
  disabled: boolean;
  onSubmit: (values: FormData) => void;
}) {
  const uiSchema: UiSchema = {
    ...(spec.uiSchema as UiSchema | undefined),
    'ui:submitButtonOptions': {
      submitText: spec.submit,
      ...((spec.uiSchema as UiSchema | undefined)?.['ui:submitButtonOptions'] ?? {}),
    },
  };
  return (
    <div className="playground-form mt-2.5">
      <Form
        schema={spec.schema as RJSFSchema}
        uiSchema={uiSchema}
        validator={validator}
        disabled={disabled}
        showErrorList={false}
        // Let ajv own validation so errors render inline with the card's
        // styling instead of the browser's native required-field tooltip.
        noHtml5Validate
        focusOnFirstError
        onSubmit={({ formData }) => {
          onSubmit((formData ?? {}) as FormData);
        }}
      />
    </div>
  );
}
